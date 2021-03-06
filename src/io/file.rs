/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Pippin: data access for repositories.

use std::path::{Path, PathBuf};
use std::io::{Read, Write};
use std::fs::{File, OpenOptions};
use std::ops::Add;

use vec_map::{VecMap, Entry};

use io::RepoIO;
use error::{Result, ReadOnly};


// —————  Partition  —————

/// Data structure used in a `RepoFileIO` to actually store file paths.
#[derive(Clone, Debug, Default)]
pub struct PartPaths {
    // First key is snapshot number. Value is (if found) a path to the snapshot
    // file and a map of log paths.
    // Key of internal map is log number. Value is a path to the log file.
    paths: VecMap<(Option<PathBuf>, VecMap<PathBuf>)>
}
impl PartPaths {
    /// Create an empty structure.
    pub fn new() -> PartPaths { PartPaths { paths: VecMap::new() } }
    
    fn ss_len(&self) -> usize {
        self.paths.keys().next_back().map(|x| x+1).unwrap_or(0)
    }
    fn ss_cl_len(&self, ss_num: usize) -> usize {
        self.paths.get(ss_num) // Option<(_, VecMap<PathBuf>)>
            .and_then(|&(_, ref logs)| logs.keys().next_back())
            .map(|x| x+1).unwrap_or(0)
    }
    
    /// Count the snapshot files present.
    pub fn num_ss_files(&self) -> usize {
        self.paths.values().filter(|v| v.0.is_some()).count()
    }
    /// Count the log files present.
    pub fn num_cl_files(&self) -> usize {
        // #0018: could use `.sum()` but see https://github.com/rust-lang/rust/issues/27739
        self.paths.values().map(|v| v.1.len()).fold(0, Add::add)
    }
    
    /// Returns a reference to the path of a snapshot file path, if found.
    pub fn get_ss(&self, ss: usize) -> Option<&Path> {
        self.paths.get(ss).and_then(|&(ref p, _)| p.as_ref().map(|path| path.as_path()))
    }
    /// Returns a reference to the path of a log file, if found.
    pub fn get_cl(&self, ss: usize, cl: usize) -> Option<&Path> {
        self.paths.get(ss)     // Option<(_, VecMap<PathBuf>)>
            .and_then(|&(_, ref logs)| logs.get(cl))    // Option<PathBuf>
            .map(|p| p.as_path())
    }
    
    /// Add a path to the list of known files. This does not do any checking.
    /// 
    /// If a file with this snapshot number was previously known, it is replaced
    /// and `true` returned; otherwise `false` is returned.
    pub fn insert_ss(&mut self, ss_num: usize, path: PathBuf) -> bool {
        match self.paths.entry(ss_num) {
            Entry::Occupied(e) => {
                let has_previous = e.get().0.is_some();
                e.into_mut().0 = Some(path);
                has_previous
            }
            Entry::Vacant(e) => {
                e.insert((Some(path), VecMap::new()));
                false
            },
        }
    }
    /// Add a path to the list of known files. This does not do any checking.
    /// 
    /// If a file with this snapshot number and commit-log number was
    /// previously known, it is replaced and `true` returned; otherwise `false`
    /// is returned.
    pub fn insert_cl(&mut self, ss_num: usize, cl_num: usize, path: PathBuf) -> bool {
        self.paths.entry(ss_num)
                .or_insert_with(|| (None, VecMap::new()))
                .1.insert(cl_num, path) /* returns old value */
                .is_some() /* i.e. something was replaced */
    }
}

/// Remembers a set of file names associated with a partition, opens read
/// and write streams on these and creates new partition files.
#[derive(Debug, Clone)]
pub struct RepoFileIO {
    readonly: bool,
    // Appended with snapshot/log number and extension to get a file path
    prefix: PathBuf,
    paths: PartPaths,
}

impl RepoFileIO {
    /// Create for a new repository. This is equivalent to calling `for_paths` with
    /// `PartPaths::new()` as the second argument.
    /// 
    /// *   `prefix` is a dir + partial-file-name; it is appended with
    ///     something like `-ss1.pip` or `-ss2-lf3.piplog` to get a file name
    pub fn new<P: Into<PathBuf>>(prefix: P) -> RepoFileIO {
        Self::for_paths(prefix, PartPaths::new())
    }
    
    /// Create a partition IO with paths to some existing files.
    /// 
    /// *   `prefix` is a dir + partial-file-name; it is appended with
    ///     something like `-ss1.pip` or `-ss2-lf3.piplog` to get a file name
    /// *   `paths` is a list of paths of all known partition files
    pub fn for_paths<P: Into<PathBuf>>(prefix: P, paths: PartPaths) -> RepoFileIO
    {
        let prefix = prefix.into();
        trace!("New RepoFileIO; prefix: {}, ss_len: {}", prefix.display(), paths.ss_len());
        RepoFileIO {
            readonly: false,
            prefix: prefix,
            paths: paths,
        }
    }
    
    /// Get property: is this readonly? If this is readonly, file creation and modification
    /// through this object will be inhibited (operations will return a `ReadOnly` error).
    pub fn readonly(&self) -> bool {
        self.readonly
    }
    
    /// Set readonly. If this is readonly, file creation and modification through this object will
    /// be inhibited (operations will return a `ReadOnly` error).
    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }
    
    /// Get a reference to the prefix
    pub fn prefix(&self) -> &Path {
        &self.prefix
    }
    /// Get a reference to the internal store of paths
    pub fn paths(&self) -> &PartPaths {
        &self.paths
    }
    /// Get a mutable reference to the internal store of paths
    pub fn mut_paths(&mut self) -> &mut PartPaths {
        &mut self.paths
    }
}

impl RepoIO for RepoFileIO {
    fn ss_len(&self) -> usize {
        self.paths.ss_len()
    }
    fn ss_cl_len(&self, ss_num: usize) -> usize {
        self.paths.ss_cl_len(ss_num)
    }
    
    fn has_ss(&self, ss_num: usize) -> bool {
        self.paths.paths.get(ss_num).map(|&(ref p, _)| p.is_some()).unwrap_or(false)
    }
    
    fn read_ss<'a>(&'a self, ss_num: usize) -> Result<Option<Box<Read+'a>>> {
        // Cannot replace `match` with `map` since `try!()` cannot be used in a closure
        Ok(match self.paths.paths.get(ss_num) {
            Some(&(ref p, _)) => {
                if let Some(ref path) = *p {
                    trace!("Reading snapshot file: {}", path.display());
                    Some(Box::new(File::open(path)?))
                } else {
                    None
                }
            },
            None => None
        })
    }
    
    fn read_ss_cl<'a>(&'a self, ss_num: usize, cl_num: usize) -> Result<Option<Box<Read+'a>>> {
        Ok(match self.paths.paths.get(ss_num).and_then(|&(_, ref logs)| logs.get(cl_num)) {
            Some(p) => {
                trace!("Reading log file: {}", p.display());
                Some(Box::new(File::open(p)?))
            },
            None => None,
        })
    }
    
    fn new_ss<'a>(&'a mut self, ss_num: usize) -> Result<Option<Box<Write+'a>>> {
        if self.readonly {
            return ReadOnly::err();
        }
        let mut p = self.prefix.as_os_str().to_os_string();
        p.push(format!("-ss{}.pip", ss_num));
        let p = PathBuf::from(p);
        if self.paths.paths.get(ss_num).map_or(false, |&(ref p, _)| p.is_some()) || p.exists() {
            // File already exists in internal map or on filesystem
            return Ok(None);
        }
        trace!("Creating snapshot file: {}", p.display());
        let stream = File::create(&p)?;
        match self.paths.paths.entry(ss_num) {
            Entry::Occupied(mut entry) => { entry.get_mut().0 = Some(p); },
            Entry::Vacant(entry) => { entry.insert((Some(p), VecMap::new())); },
        };
        Ok(Some(Box::new(stream)))
    }
    
    fn append_ss_cl<'a>(&'a mut self, ss_num: usize, cl_num: usize) -> Result<Option<Box<Write+'a>>> {
        if self.readonly {
            return ReadOnly::err();
        }
        Ok(match self.paths.paths.get(ss_num).and_then(|&(_, ref logs)| logs.get(cl_num)) {
            Some(p) => {
                trace!("Appending to log file: {}", p.display());
                Some(Box::new(OpenOptions::new().write(true).append(true).open(p)?))
            },
            None => None
        })
    }
    fn new_ss_cl<'a>(&'a mut self, ss_num: usize, cl_num: usize) -> Result<Option<Box<Write+'a>>> {
        if self.readonly {
            return ReadOnly::err();
        }
        let mut logs = &mut self.paths.paths.entry(ss_num).or_insert_with(|| (None, VecMap::new())).1;
        let mut p = self.prefix.as_os_str().to_os_string();
        p.push(format!("-ss{}-cl{}.piplog", ss_num, cl_num));
        let p = PathBuf::from(p);
        if logs.contains_key(cl_num) || p.exists() {
            // File already exists in internal map or on filesystem
            return Ok(None);
        }
        trace!("Creating log file: {}", p.display());
        let stream = OpenOptions::new().create(true).write(true).append(true).open(&p)?;
        logs.insert(cl_num, p);
        Ok(Some(Box::new(stream)))
    }
}
