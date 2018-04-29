use std::hash::{Hash,Hasher};
use std::collections::hash_map::DefaultHasher;
use std::sync::atomic::{AtomicPtr,Ordering};
use std::option::Option;
use std::ptr::null_mut;
use std::intrinsics::type_name;

pub trait TrieData: Clone + Copy + Eq + PartialEq {}

impl<T> TrieData for T where T: Clone + Copy + Eq + PartialEq {}

pub trait TrieKey: Clone + Copy + Eq + PartialEq + Hash {}
impl<T> TrieKey for T where T: Clone + Copy + Eq + PartialEq + Hash {}

type ANode<K,V> = Vec<AtomicPtr<Node<K,V>>>;

enum Node<K,V> {
    SNode {
        hash: u64,
        key: K,
        val: V,
        txn: AtomicPtr<Node<K,V>>
    },
    ANode(ANode<K,V>),
    NoTxn,
    FSNode,
    FVNode,
    FNode {
        frozen: AtomicPtr<Node<K,V>>
    },
    ENode {
        parent: AtomicPtr<Node<K,V>>,
        parentpos: u8,
        narrow: AtomicPtr<Node<K,V>>,
        hash: u64,
        level: u8,
        wide: AtomicPtr<Node<K,V>>,
    }
}

fn hash<T>(obj: T) -> u64
where
    T: Hash {
    let mut hasher = DefaultHasher::new();
    obj.hash(&mut hasher);
    hasher.finish()
}

pub struct LockfreeTrie<K: TrieKey, V: TrieData> {
    root: AtomicPtr<Node<K,V>>
}

fn makeanode<K,V>(len: usize) -> ANode<K,V> {
    let mut a: ANode<K,V> = Vec::with_capacity(len);

    for i in 0..len { a.push(AtomicPtr::new(null_mut())); }
    println!("created new vec: {:p}", a.as_mut_ptr());
    return a;
}

/**
 * TODO: fix memory leaks and use atomic_ref or crossbeam crates
 */
fn alloc<T>(thing: T) -> *mut T {
    let p = Box::into_raw(box thing);
    println!("created new {:p} ({})", p, unsafe {type_name::<T>()});
    return p;
}

impl<K: TrieKey, V: TrieData> LockfreeTrie<K,V> {
    pub fn new() -> Self {
        LockfreeTrie {
            root: AtomicPtr::new(alloc(Node::ANode(makeanode(16))))
        }
    }

    fn _freeze(nnode: &mut Node<K,V>) -> () {
        if let Node::ANode(ref cur) = nnode {
            let mut i = 0;
            println!("_freeze({:p} in {:p})", cur.as_ptr(), nnode);
            while i < cur.len() {
                let node = &cur[i];
                let nodeptr = node.load(Ordering::Relaxed);
                let noderef = unsafe {&mut *nodeptr};

                assert_ne!(nodeptr, nnode as *mut Node<K,V>, "i == {}", i);

                i += 1;
                if nodeptr.is_null() {
                    if node.compare_and_swap(nodeptr, alloc(Node::FVNode), Ordering::Relaxed) != nodeptr {
                        i -= 1;
                    }
                } else if let Node::SNode { ref txn, .. } = noderef {
                    let txnptr = txn.load(Ordering::Relaxed);
                    let txnref = unsafe {&mut *txnptr};
                    if let Node::NoTxn = txnref {
                        if txn.compare_and_swap(txnptr, alloc(Node::FSNode), Ordering::Relaxed) != txnptr {
                            i -= 1;
                        }
                    } else if let Node::FSNode = txnref {
                    } else {
                        node.compare_and_swap(nodeptr, txnptr, Ordering::Relaxed);
                        i -= 1;
                    }
                } else if let Node::ANode(ref an) = noderef {
                    let fnode = alloc(Node::FNode { frozen: AtomicPtr::new(noderef) });
                    node.compare_and_swap(nodeptr, fnode, Ordering::Relaxed);
                    i -= 1;
                } else if let Node::FNode { ref frozen } = noderef {
                    LockfreeTrie::_freeze(unsafe {&mut *frozen.load(Ordering::Relaxed)});
                } else if let Node::ENode { .. } = noderef {
                    LockfreeTrie::_complete_expansion(noderef);
                    i -= 1;
                }
            }
        } else {
            panic!("CORRUPTION: nnode is not an ANode")
        }
    }

    fn _copy(an: &ANode<K,V>, wide: &mut Node<K,V>, lev: u64) -> () {
        for node in an {
            match unsafe {&*node.load(Ordering::Relaxed)} {
                Node::FNode { ref frozen } => {
                    let frzref = unsafe {&*frozen.load(Ordering::Relaxed)};
                    if let Node::ANode(ref an2) = frzref {
                        LockfreeTrie::_copy(an2, wide, lev + 4);
                    } else {
                        panic!("CORRUPTION: FNode contains non-ANode")
                    }
                },
                Node::SNode { hash, key, val, txn } => {
                    LockfreeTrie::_insert(*key, *val, *hash, lev as u8, wide, None);
                },
                _ => { /* ignore */ }
            }
        }
    }

    fn _complete_expansion(enode: &mut Node<K,V>) -> () {
        if let Node::ENode { ref parent, parentpos, ref narrow, level, wide: ref mut _wide, .. } = enode {
            let narrowptr = narrow.load(Ordering::Relaxed);
            LockfreeTrie::_freeze(unsafe {&mut *narrowptr} );
            let mut widenode = alloc(Node::ANode(makeanode(16)));
            if let Node::ANode(ref an) = unsafe {&*narrowptr} {
                LockfreeTrie::_copy(an, unsafe {&mut *widenode}, *level as u64);
            } else {
                panic!("CORRUPTION: narrow is not an ANode")
            }
            if _wide.compare_and_swap(null_mut(), widenode, Ordering::Relaxed) != null_mut() {
                let _wideptr = _wide.load(Ordering::Relaxed);
                if let Node::ANode(ref an) = unsafe {&mut *_wideptr} {
                    widenode = unsafe {&mut *_wideptr};
                } else {
                    panic!("_wide is not an ANode")
                }
            }
            let parentref = unsafe {&*parent.load(Ordering::Relaxed)};
            if let Node::ANode(ref an) = parentref {
                let anptr = &an[*parentpos as usize];
                anptr.compare_and_swap(enode, widenode, Ordering::Relaxed);
            } else {
                panic!("CORRUPTION: parent is not an ANode")
            }
        } else {
            // this should never be reached
            panic!("CORRUPTION: enode is not an ENode")
        }
    }

    fn _create_anode(old: Node<K,V>, sn: Node<K,V>, lev: u8) -> ANode<K,V> {
        let mut v = makeanode(4);

        if let Node::SNode { hash: h_old, .. } = old {
            let old_pos = (h_old >> lev) as usize & (v.len() - 1);
            if let Node::SNode { hash: h_sn, .. } = sn {
                let sn_pos = (h_sn >> lev) as usize & (v.len() - 1);
                if old_pos == sn_pos {
                    v[old_pos] = AtomicPtr::new(alloc(Node::ANode(LockfreeTrie::_create_anode(old, sn, lev + 4))));
                } else {
                    v[old_pos] = AtomicPtr::new(alloc(old));
                    v[sn_pos] = AtomicPtr::new(alloc(sn));
                }
            } else {
                panic!("CORRUPTION: expected SNode");
            }
        } else {
            panic!("CORRUPTION: expected SNode");
        }
        println!("_create_anode() = {:p}", v.as_ptr());
        return v;
    }

    fn _insert(key: K, val: V, h: u64, lev: u8,
        cur: &mut Node<K,V>,
        prev: Option<&mut Node<K,V>>) -> bool {

        if let Node::ANode(ref mut cur2) = cur {
            let pos = (h >> lev) as usize & (cur2.len() - 1);
            let old = &cur2[pos];
            let oldptr = old.load(Ordering::Relaxed);
            let oldref = unsafe {&mut *oldptr};

            if oldptr.is_null() {
                let sn = alloc(Node::SNode {
                    hash: h,
                    key: key,
                    val: val,
                    txn: AtomicPtr::new(alloc(Node::NoTxn))
                });
                if old.compare_and_swap(oldptr, sn, Ordering::Relaxed) == oldptr {
                    println!("inserted new SNode at pos {}, level {}", pos, lev);
                    true
                } else {
                    LockfreeTrie::_insert(key, val, h, lev, cur, prev)
                }
            } else if let Node::ANode(ref mut an) = oldref {
                LockfreeTrie::_insert(key, val, h, lev + 4, oldref, Some(cur))
            } else if let Node::SNode { hash: _hash, key: _key, val: _val, ref mut txn } = oldref {
                let txnptr = txn.load(Ordering::Relaxed);
                let txnref = unsafe {&*txnptr};

                if let Node::NoTxn = txnref {
                    if *_key == key {
                        let sn = alloc(Node::SNode {
                            hash: h,
                            key: key,
                            val: val,
                            txn: AtomicPtr::new(alloc(Node::NoTxn))
                        });
                        if txn.compare_and_swap(txnptr, sn, Ordering::Relaxed) == txnptr {
                            old.compare_and_swap(oldptr, sn, Ordering::Relaxed);
                            println!("replacing old value at pos {}, level {}", pos, lev);
                            true
                        } else {
                            LockfreeTrie::_insert(key, val, h, lev, cur, prev)
                        }
                    } else if cur2.len() == 4 {
                        if let Some(prevref) = prev {
                            if let Node::ANode(ref mut prev2) = prevref {
                                let ppos = (h >> (lev - 4)) as usize & (prev2.len() - 1);
                                let prev2aptr = &prev2[ppos];
                                let en = alloc(Node::ENode {
                                    parent: AtomicPtr::new(prevref),
                                    parentpos: ppos as u8,
                                    narrow: AtomicPtr::new(cur),
                                    hash: h,
                                    level: lev,
                                    wide: AtomicPtr::new(null_mut())
                                });
                                if prev2aptr.compare_and_swap(cur, en, Ordering::Relaxed) == cur {
                                    println!("expanding short ANode at pos {}, lev {}", ppos, lev - 4);
                                    LockfreeTrie::_complete_expansion(unsafe{&mut *en});
                                    println!("completed expansion");
                                    if let Node::ENode { ref wide, .. } = unsafe{&mut *en} {
                                        let wideref = unsafe {&mut *wide.load(Ordering::Relaxed)};
                                        LockfreeTrie::_insert(key, val, h, lev, wideref, Some(prevref))
                                    } else {
                                        // should not be reached
                                        panic!("CORRUPTION: en is not an ENode")
                                    }
                                } else {
                                    LockfreeTrie::_insert(key, val, h, lev, cur, Some(prevref))
                                }
                            } else {
                                // should not be reached
                                panic!("CORRUPTION: prevref is not an ANode")
                            }
                        } else {
                            panic!("ERROR: prev is None")
                        }
                    } else {
                        let an = alloc(Node::ANode(LockfreeTrie::_create_anode(
                            Node::SNode {
                                hash: *_hash,
                                key: *_key,
                                val: *_val,
                                txn: AtomicPtr::new(alloc(Node::NoTxn))
                            },
                            Node::SNode {
                                hash: h,
                                key: key,
                                val: val,
                                txn: AtomicPtr::new(alloc(Node::NoTxn))
                            }, lev + 4)));
                        if txn.compare_and_swap(txnptr, an, Ordering::Relaxed) == txnptr {
                            old.compare_and_swap(oldptr, an, Ordering::Relaxed);
                            if let Node::ANode(ref an2) = unsafe {&*an} {
                                let an2ptr = an2.as_ptr();
                                for n in an2 {
                                    let p = n.load(Ordering::Relaxed);
                                    println!("{:p} in {:p} contains {:p}", an2ptr, an, p);
                                }
                            }
                            println!("created new ANode (4) at pos {}, level {}", pos, lev);
                            true
                        } else {
                            LockfreeTrie::_insert(key, val, h, lev, cur, prev)
                        }
                    }
                } else if let Node::FSNode = txnref {
                    false
                } else {
                    old.compare_and_swap(oldptr, txnptr, Ordering::Relaxed);
                    LockfreeTrie::_insert(key, val, h, lev, cur, prev)
                }
            } else {
                if let Node::ENode { .. } = oldref {
                    LockfreeTrie::_complete_expansion(oldref);
                }
                false
            }
        } else {
            // should not be reached
            panic!("CORRUPTION: curref is not an ANode")
        }
    }

    pub fn insert(&mut self, key: K, val: V) -> bool {
        LockfreeTrie::_insert(key, val, hash(key), 0, unsafe {&mut *self.root.load(Ordering::Relaxed)}, None)
            || self.insert(key, val)
    }

    fn _lookup<'a>(key: &K, h: u64, lev: u8, cur: &'a AtomicPtr<Node<K,V>>) -> Option<&'a V> {
        let curptr = cur.load(Ordering::Relaxed);
        let curref = unsafe {&mut *curptr};
        if let Node::ANode(ref mut cur2) = curref {
            let pos = (h >> lev) as usize & (cur2.len() - 1);
            let oldptr = cur2[pos].load(Ordering::Relaxed);
            let oldref = unsafe {&*oldptr};

            if oldptr.is_null() {
                None
            } else if let Node::FVNode = oldref {
                None
            } else if let Node::ANode(ref an) = oldref {
                LockfreeTrie::_lookup(key, h, lev + 4, &cur2[pos])
            } else if let Node::SNode { key: _key, val, .. } = oldref {
                if *_key == *key {
                    Some(val)
                } else {
                    None
                }
            } else if let Node::ENode { narrow, .. } = oldref {
                LockfreeTrie::_lookup(key, h, lev + 4, narrow)
            } else if let Node::FNode { frozen } = oldref {
                LockfreeTrie::_lookup(key, h, lev + 4, frozen)
            } else {
                panic!("CORRUPTION: oldref is not a valid node")
            }
        } else {
            panic!("CORRUPTION: cur is not a pointer to ANode")
        }
    }

    pub fn lookup(&self, key: &K) -> Option<&V> {
        LockfreeTrie::_lookup(key, hash(key), 0, &self.root)
    }
}
