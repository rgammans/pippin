//! Pippin (sync-sets) library

// because at this stage of development there's a lot of it:
#![allow(dead_code)]

// https://github.com/rust-lang/rust/issues/27790
// could be replaced with a stub function if needing to use stable releases
#![feature(vec_resize)]

extern crate crypto;
extern crate chrono;
extern crate byteorder;

use std::{io, fmt, fs};
use std::collections::HashMap;
use std::path::Path;
use std::convert::AsRef;

pub use error::{Error, Result};

pub mod error;
mod detail;

/// Version. The low 16 bits are patch number, next 16 are the minor version
/// number, the next are the major version number. The top 16 are zero.
pub const LIB_VERSION: u64 = 0x0000_0000_0000;

/*TODO: partitions/generic resources
// Method of providing partition data
pub trait DataResource {
    // This function should return some kind of data stream given a name. The
    // name might for example be a file name minus extension under some path.
    fn resolve(&mut self, name: &str) -> io::Read;
    
    // This function should create a writer to write to a new file (or other
    // store). The Write object will be destroyed after use.
    fn write(&mut self, name: &str) -> io::Write;
    
    // This function should open a file/store for appending. If the file does
    // not exist or its current length is not that given, the operation should
    // fail.
    fn append(&mut self, name: &str, len: u64) -> io::Result<io::Write>;
}*/

// Handle on a repository
pub struct Repo {
    name: String,
    // NOTE: with sequential keys VecMap could be an alternative to HashMap
    elements: HashMap<u64, Element>
}

// Non-member functions on Repo
impl Repo {
    /// Create a new repo with the given name
    pub fn new(name: String) -> Result<Repo> {
        try!(detail::validate_repo_name(&name));
        Ok(Repo{
            name: name,
            elements: HashMap::new()
        })
    }
    
    /// Load a snapshot from a stream
    pub fn load_stream(stream: &mut io::Read) -> Result<Repo> {
        let head = try!(detail::read_head(stream));
        let elts = try!(detail::read_snapshot(stream));
        //TODO: could check that we're at the end of the stream (?)
        
        Ok(Repo {
            name: head.name,
            elements: elts
        })
    }
    
    /// Load a snapshot from a file
    pub fn load_file<P: AsRef<Path>>(p: P) -> Result<Repo> {
        let mut f = try!(fs::File::open(p));
        Repo::load_stream(&mut f)
    }
    
    /// Save a snapshot to a stream
    pub fn save_stream(&self, stream: &mut io::Write) -> Result<()> {
        let head = detail::FileHeader {
            name: self.name.clone(),
            remarks: vec![],
            user_fields: vec![]
        };
        
        try!(detail::write_head(&head, stream));
        detail::write_snapshot(&self.elements, stream)
    }
    
    /// Save a snapshot to a file
    pub fn save_file<P: AsRef<Path>>(&self, p: P) -> Result<()> {
        let mut f = try!(fs::File::create(p));
        self.save_stream(&mut f)
    }
}

// Member functions on Repo — a set of elements.
//
// Each element of the set has a unique identifier and some data. This Repo
// stores the elements along with a history of their changes.
//
// This data store is optimised for the case where elements only have a small
// amount of data, verification of data integrity is important and disk writes
// should be minimised. It is designed to scale beyond available memory via
// partitioning and to allow simple backup as well as recovery of as much data
// as possible in the case that some information is lost. It is also designed
// to enable synchronisation of the data set between multiple computers over
// even low-speed network connections, such that all computers have a full
// local copy of the data and its history.
impl Repo {
    /// Get the repo name
    pub fn name(&self) -> &str { &self.name }
    
    /// Get the number of elements
    pub fn num_elts(&self) -> usize {
        self.elements.len()
    }
    
    /// Return true iff this element exists
    pub fn has_elt(&self, id: u64) -> bool {
        self.elements.contains_key(&id)
    }
    
    /// Insert an element. If overwrite is false and an element with this id
    /// exists, do nothing and return false; otherwise insert the element and
    /// return true.
    pub fn insert_elt(&mut self, id: u64, data: &[u8], overwrite: bool) -> bool {
        if !overwrite && self.elements.contains_key(&id) {
            return false;
        }
        self.elements.insert(id, Element { data: data.to_vec() });
        true
    }
    
    // TODO: list all partitions
    // TODO: check whether a particular partition is loaded
    
    // Unload all partitions, saving changes to disk
    pub fn unload_all(&mut self) {}
    
    // Commit all changes to disk
    pub fn commit_all(&mut self) {}
    
    //TODO: when partitions are introduced, item_id will be specific to the partition
    // Get an item as a byte vector. Panics on invalid item_id.
    pub fn get_item(&self, item_id: u64) -> &Element {
        self.elements.get(&item_id).unwrap()
    }
}

/* TODO: add partitioning
// A *partition* is a sub-set of the entire set such that (a) each element is
// in exactly one partition, (b) a partition is small enough to be loaded into
// memory in its entirety, (c) there is some user control over the number of
// partitions and how elements are assigned partitions and (d) each partition
// can be managed independently of other partitions.
//
// Partitions are the *only* method by which the entire set may grow beyond
// available memory, thus smart allocation of elements to partitions will be
// essential for some use-cases.
struct Partition {
    // NOTE: with sequential keys VecMap could be an alternative to HashMap
    elements: HashMap<u64, Element>
}

// Partition functions
impl Partition {
    // Unload partition, saving changes to disk
    pub fn unload(&mut self) {}
    
    // Commit changes to disk
    pub fn commit(&mut self) {}
    
    // Enumerate all item identifiers
    // TODO
    
    // Get an item as a byte vector. Panics on invalid item_id.
    pub fn get_item(&self, item_id: u64) -> &Element {
        self.elements.get(&item_id).unwrap()
    }
    
    // List all items in full (or through some filter)
    // TODO
    
    // Search for some item
    // TODO
}
*/

// Holds an element's data in memory
// TODO: replace with a trait and user-defined implementation?
#[derive(PartialEq,Eq)]
pub struct Element {
    data: Vec<u8>
}

impl fmt::Debug for Element {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "element (len {})", self.data.len())
    }
}
