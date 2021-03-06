/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Pippin: partition

use std::io::ErrorKind;
use std::collections::{HashSet, VecDeque};
use std::collections::hash_set as hs;
use std::result;
use std::ops::Deref;
use std::usize;
use std::cmp::min;

use hashindexed::{HashIndexed, Iter};

use commit::Commit;
use control::Control;
use elt::Element;
use error::{Result, TipError, PatchOp, MatchError, MergeError, OtherError, make_io_err};
use merge::{TwoWayMerge, TwoWaySolver};
use rw::header::{FileType, FileHeader, validate_repo_name, read_head, write_head};
use rw::snapshot::{read_snapshot, write_snapshot};
use rw::commitlog::{read_log, start_log, write_commit};
use state::{PartState, MutPartState, PartStateSumComparator};
use sum::Sum;


/// A *partition* is a sub-set of the entire set such that (a) each element is
/// in exactly one partition, (b) a partition is small enough to be loaded into
/// memory in its entirety, (c) there is some user control over the number of
/// partitions and how elements are assigned partitions and (d) each partition
/// can be managed independently of other partitions.
///
/// Partitions are the *only* method by which the entire set may grow beyond
/// available memory, thus smart allocation of elements to partitions will be
/// essential for some use-cases.
/// 
/// A partition is in one of three possible states: (1) unloaded, (2) loaded
/// but requiring a merge (multiple tips), (3) ready for use.
/// 
/// Terminology: a *tip* (as in *point* or *peak*) is a state without a known
/// successor. Normally there is exactly one tip, but see `is_ready`,
/// `is_loaded` and `merge_required`.
pub struct Partition<C: Control> {
    // User control trait object
    control: C,
    // Repository name. Used to identify loaded files.
    name: String,
    // Number of first snapshot file loaded (equal to ss1 if nothing is loaded)
    ss0: usize,
    // Number of latest snapshot file loaded + 1; 0 if nothing loaded and never less than ss0
    ss1: usize,
    // Known committed states indexed by statesum 
    states: HashIndexed<PartState<C::Element>, Sum, PartStateSumComparator>,
    // All states not in `states` which are known to be superceded
    ancestors: HashSet<Sum>,
    // All states without a known successor
    tips: HashSet<Sum>,
    // Commits created but not yet saved to disk. First in at front; use as queue.
    unsaved: VecDeque<Commit<C::Element>>,
}

// Methods creating a partition, loading its data or checking status
impl<C: Control> Partition<C> {
    /// Create a partition, assigning an IO provider (this can only be done at
    /// time of creation). Create a blank state in the partition, write an
    /// empty snapshot to the provided `RepoIO`, and mark self as *ready
    /// for use*.
    /// 
    /// Example:
    /// 
    /// ```
    /// use pippin::pip::{Partition, RepoIO, DefaultControl, DummyRepoIO};
    /// 
    /// let control = DefaultControl::<String, _>::new(DummyRepoIO::new());
    /// let partition = Partition::create(control, "example repo");
    /// ```
    pub fn create(mut control: C, name: &str) -> Result<Partition<C>> {
        validate_repo_name(name)?;
        let ss = 0;
        info!("Creating partiton; writing snapshot {}", ss);
        
        let state = PartState::new(control.as_mcm_ref_mut());
        let mut part = Partition {
            control: control,
            name: name.into(),
            ss0: ss,
            ss1: ss + 1,
            states: HashIndexed::new(),
            ancestors: HashSet::new(),
            tips: HashSet::new(),
            unsaved: VecDeque::new(),
        };
        let header = part.make_header(FileType::Snapshot(0))?;
        
         if let Some(mut writer) = part.control.io_mut().new_ss(ss)? {
            write_head(&header, &mut writer)?;
            write_snapshot(&state, &mut writer)?;
        } else {
            return make_io_err(ErrorKind::AlreadyExists, "snapshot already exists");
        }
        
        part.tips.insert(state.statesum().clone());
        part.states.insert(state);
        
        Ok(part)
    }
    
    /// Open a partition, assigning an IO provider (this can only be done at
    /// time of creation).
    /// 
    /// If `read_data` is true, the latest state is read into memory immediately.
    /// Otherwise, initialise the partition without reading data (currently requires reading a
    /// snapshot header). In this case the partition will not be *ready to use* until data is
    /// loaded with one of the load operations. Until then most operations will fail.
    /// 
    /// Example:
    /// 
    /// ```no_run
    /// use std::path::Path;
    /// use pippin::pip::{Partition, DefaultControl, part_from_path};
    /// 
    /// let path = Path::new("./my-partition");
    /// let io = part_from_path(path).unwrap();
    /// let control = DefaultControl::<String, _>::new(io);
    /// let partition = Partition::open(control, true);
    /// ```
    pub fn open(control: C, read_data: bool) -> Result<Partition<C>> {
        trace!("Opening partition");
        // We need to read a header for classification purposes
        
        let ss_len = control.io().ss_len();
        for ss in (0..ss_len).rev() {
            debug!("Partition: reading snapshot {}", ss);
            let result = if let Some(mut ssf) = control.io().read_ss(ss)? {
                let head = read_head(&mut *ssf)?;
                trace!("Partition: name: {}", head.name);
                
                let state = if read_data {
                    Some(read_snapshot(&mut *ssf, head.ftype.ver())?)
                } else {
                    None
                };
                
                Some((head.name, state))
            } else {
                warn!("Partition: missing snapshot {}", ss);
                None
            };
            if let Some((name, opt_state)) = result {
                let mut part = Partition {
                    control,
                    name,
                    ss0: 0,
                    ss1: 0,
                    states: HashIndexed::new(),
                    ancestors: HashSet::new(),
                    tips: HashSet::new(),
                    unsaved: VecDeque::new(),
                };
                
                if let Some(state) = opt_state {
                    part.tips.insert(state.statesum().clone());
                    for parent in state.parents() {
                        part.ancestors.insert(parent.clone());
                    }
                    part.states.insert(state);
                    part.control.snapshot_policy().reset();
                    part.ss0 = ss;
                    for ss2 in ss..ss_len {
                        part.read_commits_for_ss(ss2)?;
                    }
                    part.ss1 = ss_len;
                }
                
                return Ok(part);
            }
        }
        OtherError::err("no snapshot found for first partition")
    }
    
    /// Get the repo name, contained in each file's header.
    pub fn name(&self) -> &str {
        &self.name
    }
    
    /// Load all history. Shortcut for `load_range(0, usize::MAX, control)`.
    pub fn load_all(&mut self) -> Result<()> {
        self.load_range(0, usize::MAX)
    }
    /// Load latest state from history (usually including some historical
    /// data). Shortcut for `load_range(usize::MAX, usize::MAX, control)`.
    pub fn load_latest(&mut self) -> Result<()> {
        self.load_range(usize::MAX, usize::MAX)
    }
    
    /// Load snapshots `ss` where `ss0 <= ss < ss1`, and all log files for each
    /// snapshot loaded. If `ss0` is beyond the latest snapshot found, it will
    /// be reduced to the number of the last snapshot. `ss1` may be large. For
    /// example, `(0, usize::MAX)` means load everything available and
    /// `(usize::MAX, usize::MAX)` means load only the latest state.
    /// 
    /// Special behaviour: if some snapshots are already loaded and the range
    /// does not overlap with this range, all snapshots in between will be
    /// loaded.
    /// 
    /// TODO: allow loading new & extended log files when snapshot is already loaded.
    pub fn load_range(&mut self, ss0: usize, ss1: usize) -> Result<()> {
        // We have to consider several cases: nothing previously loaded, that
        // we're loading data older than what was previously loaded, or newer,
        // or even overlapping. The algorithm we use is:
        // 
        //  while ss0 > 0 and not has_ss(ss0), ss0 -= 1
        //  if ss0 == 0 and not has_ss(0), assume initial state
        //  for ss in ss0..ss1:
        //      if this snapshot was already loaded, skip
        //      load snapshot if found, skip if not
        //      load all logs found and rebuild states, aborting if parents are missing
        
        // Input arguments may be greater than the available snapshot numbers. Clamp:
        let ss_len = self.control.io().ss_len();
        let mut ss0 = min(ss0, if ss_len > 0 { ss_len - 1 } else { ss_len });
        let mut ss1 = min(ss1, ss_len);
        // If data is already loaded, we must load snapshots between it and the new range too:
        if self.ss1 > self.ss0 {
            if ss0 > self.ss1 { ss0 = self.ss1; }
            if ss1 < self.ss0 { ss1 = self.ss0; }
        }
        // If snapshot files are missing, we need to load older files:
        while ss0 > 0 && !self.control.io().has_ss(ss0) { ss0 -= 1; }
        
        if ss0 == 0 && !self.control.io().has_ss(ss0) {
            // No initial snapshot; assume a blank state
            let state = PartState::new(self.control.as_mcm_ref_mut());
            self.tips.insert(state.statesum().clone());
            self.states.insert(state);
        }
        
        let mut require_ss = false;
        for ss in ss0..ss1 {
            // If already loaded, skip this snapshot:
            if self.ss0 <= ss && ss < self.ss1 { continue; }
            let at_tip = ss >= self.ss1;
            
            debug!("Partition {}: reading snapshot {}", self.name, ss);
            let opt_result = if let Some(mut r) = self.control.io().read_ss(ss)? {
                let head = read_head(&mut r)?;
                let state = read_snapshot(&mut r, head.ftype.ver())?;
                Some((head, state))
            } else {
                warn!("Partition {}: missing snapshot {}", self.name, ss);
                None
            };
            
            if let Some((header, state)) = opt_result {
                self.verify_header(header)?;
                
                if !self.ancestors.contains(state.statesum()) {
                    self.tips.insert(state.statesum().clone());
                }
                for parent in state.parents() {
                    if !self.states.contains(parent) {
                        self.ancestors.insert(parent.clone());
                    }
                }
                // TODO: check that classification in state equals that of this partition? (Already done in this case.)
                self.states.insert(state);
                
                require_ss = false;
                if at_tip {
                    self.control.snapshot_policy().reset();
                }
            } else {
                // Missing snapshot; if at head require a new one
                require_ss = at_tip;
            }
            
            self.read_commits_for_ss(ss)?;
            if at_tip {
                self.ss1 = ss + 1;
            }
        }
        
        if ss0 < self.ss0 {
            // Older history was loaded. In this case we can only update ss0
            // once all older snapshots have been loaded. If there was a failure
            // and retry, some snapshots could be reloaded unnecessarily.
            self.ss0 = ss0;
        }
        assert!(self.ss0 <= ss1 && ss1 <= self.ss1);
        
        if require_ss {
            self.control.snapshot_policy().force_snapshot();
        }
        Ok(())
    }
    
    // Read commit logs for a snapshot
    fn read_commits_for_ss(&mut self, ss: usize) -> Result<()> {
        let mut queue = vec![];
        for cl in 0..self.control.io().ss_cl_len(ss) {
            debug!("Partition {}: reading commit log {}-{}", self.name, ss, cl);
            let opt_header = if let Some(mut r) = self.control.io().read_ss_cl(ss, cl)? {
                let header = read_head(&mut r)?;
                read_log(&mut r, &mut queue, header.ftype.ver())?;
                Some(header)
            } else {
                warn!("Partition {}: missing commit log {}-{}", self.name, ss, cl);
                None
            };
            if let Some(header) = opt_header {
                self.verify_header(header)?;
            }
        }
        for commit in queue {
            self.add_commit(commit)?;
        }
        Ok(())
    }
    
    /// The oldest snapshot number loaded
    pub fn oldest_ss_loaded(&self) -> usize {
        self.ss0
    }
    
    /// Returns true when elements have been loaded (i.e. there is at least one
    /// tip; see also `is_ready` and `merge_required`).
    pub fn is_loaded(&self) -> bool {
        !self.tips.is_empty()
    }
    
    /// Returns true when ready for use (this is equivalent to
    /// `self.is_loaded() && !self.merge_required()`, i.e. there is exactly
    /// one tip).
    pub fn is_ready(&self) -> bool {
        self.tips.len() == 1
    }
    
    /// Returns true while a merge is required (i.e. there is more than one
    /// tip).
    /// 
    /// Returns false if not ready or no tip is found as well as when a single
    /// tip is present and ready to use.
    pub fn merge_required(&self) -> bool {
        self.tips.len() > 1
    }
    
    // Verify values in a header.
    fn verify_header(&mut self, header: FileHeader) -> Result<()> {
        if self.name != header.name {
            return OtherError::err("repository name does not match when loading (wrong repo?)");
        }
        
        self.control.read_header(&header)?;
        
        Ok(())
    }
    
    /// Create a header
    fn make_header(&mut self, file_type: FileType) -> Result<FileHeader> {
        let mut header = FileHeader {
            ftype: file_type,
            name: self.name.clone(),
            user: vec![],
        };
        let user_fields = self.control.make_user_data(&header)?;
        header.user = user_fields;
        Ok(header)
    }
    
    /// Unload data from memory. Note that unless `force == true` the operation
    /// will fail if any changes have not yet been saved to disk.
    /// 
    /// Returns true if data was unloaded, false if not (implies `!force` and 
    /// that unsaved changes exist).
    pub fn unload(&mut self, force: bool) -> bool {
        trace!("Unloading partition {} data", self.name);
        if force || self.unsaved.is_empty() {
            self.states.clear();
            self.ancestors.clear();
            self.tips.clear();
            true
        } else {
            false
        }
    }
    
    /// Consume the `Partition` and return the held `RepoIO`.
    /// 
    /// This destroys all states held internally, but states may be cloned
    /// before unwrapping. Since `Element`s are copy-on-write, cloning
    /// shouldn't be too expensive.
    pub fn unwrap_control(self) -> C {
        self.control
    }
}

// Methods accessing or modifying a partition's data
impl<C: Control> Partition<C> {
    /// Get a reference to the PartState of the current tip. You can read
    /// this directly or make a clone in order to make your modifications.
    /// 
    /// This operation will fail if no data has been loaded yet or if a merge
    /// is required (i.e. it fails if the number of tips is not exactly one).
    pub fn tip(&self) -> result::Result<&PartState<C::Element>, TipError> {
        Ok(self.states.get(self.tip_key()?).unwrap())
    }
    
    /// Get the state-sum (key) of the tip. Fails when `tip()` fails.
    pub fn tip_key(&self) -> result::Result<&Sum, TipError> {
        if self.tips.len() == 1 {
            Ok(self.tips.iter().next().unwrap())
        } else if self.tips.is_empty() {
            Err(TipError::NotReady)
        } else {
            Err(TipError::MergeRequired)
        }
    }
    
    /// Get the number of tips.
    pub fn tips_len(&self) -> usize {
        self.tips.len()
    }
    
    /// Get an iterator over tips.
    pub fn tips_iter(&self) -> TipIter {
        TipIter { iter: self.tips.iter() }
    }
    
    /// Get the set of all tips. Is empty before loading and has more than one
    /// entry when `merge_required()`. May be useful for merging.
    pub fn tips(&self) -> &HashSet<Sum> {
        &self.tips
    }
    
    /// Get the number of states.
    /// 
    /// Tips are a subset of states, so `tips_len() <= states_len()`.
    pub fn states_len(&self) -> usize {
        self.states.len()
    }
    
    /// Iterate over all states which have been loaded (see `load_...` functions).
    /// 
    /// Items are unordered (actually, they follow the order of an internal
    /// hash map, which is randomised and usually different each time the
    /// program is loaded).
    pub fn states_iter(&self) -> StateIter<C::Element> {
        StateIter { iter: self.states.iter(), tips: &self.tips }
    }
    
    /// Get a read-only reference to a state by its statesum, if found.
    /// 
    /// If you want to keep a copy, clone it.
    pub fn state(&self, key: &Sum) -> Option<&PartState<C::Element>> {
        self.states.get(key)
    }
    
    /// Try to find a state given a string representation of the key (as a byte array).
    /// 
    /// Like git, we accept partial keys (so long as they uniquely resolve a key).
    pub fn state_from_string(&self, string: String) -> Result<&PartState<C::Element>, MatchError> {
        let string = string.to_uppercase().replace(" ", "");
        let mut matching: Option<&Sum> = None;
        for state in self.states.iter() {
            if state.statesum().matches_string(string.as_bytes()) {
                if let Some(prev) = matching {
                    return Err(MatchError::MultiMatch(
                        prev.as_string(false), state.statesum().as_string(false)));
                } else {
                    matching = Some(state.statesum());
                }
            }
        }
        if let Some(m) = matching {
            Ok(self.states.get(m).unwrap())
        } else {
            Err(MatchError::NoMatch)
        }
    }
    
    /// Merge all latest states into a single tip.
    /// This is a convenience wrapper around `merge_two(...)`.
    /// 
    /// Example:
    /// 
    /// ```no_run
    /// use std::path::Path;
    /// use pippin::pip::{Partition, DefaultControl, part_from_path,
    ///         TwoWaySolverChain, AncestorSolver2W, RenamingSolver2W};
    /// 
    /// let path = Path::new("./my-partition");
    /// let io = part_from_path(path).unwrap();
    /// let control = DefaultControl::<String, _>::new(io);
    /// let mut partition = Partition::open(control, true)
    ///         .expect("failed to open partition");
    /// 
    /// // Customise with your own solver:
    /// let ancestor_solver = AncestorSolver2W::new();
    /// let renaming_solver = RenamingSolver2W::new();
    /// let solver = TwoWaySolverChain::new(&ancestor_solver, &renaming_solver);
    /// 
    /// partition.merge(&solver, true).expect("merge failed");
    /// ```
    /// 
    /// This works through all 'tip' states in an order determined by a
    /// `HashSet`'s random keying, thus the exact result may not be repeatable
    /// if the program were run multiple times with the same initial state.
    /// 
    /// If `auto_load` is true, additional history will be loaded as necessary
    /// to find a common ancestor.
    pub fn merge<S: TwoWaySolver<C::Element>>(&mut self, solver: &S, auto_load: bool) -> Result<()> {
        let mut start_ss = self.ss0;
        while self.tips.len() > 1 {
            if start_ss < self.ss0 {
                let ss0 = self.ss0;
                self.load_range(start_ss, ss0)?;
            }
            
            let (tip1, tip2): (Sum, Sum) = {
                // We sort tips in order to make the operation deterministic.
                let mut tips: Vec<_> = self.tips.iter().collect();
                tips.sort();
                (tips[0].clone(), tips[1].clone())
            };
            trace!("Partition {}: attempting merge of tips {} and {}", self.name, &tip1, &tip2);
            let c = match self.merge_two(&tip1, &tip2) {
                Ok(merge) => merge.solve_inline(solver).make_commit(self.control.as_mcm_ref()),
                Err(MergeError::NoCommonAncestor) if auto_load && self.ss0 > 0 => {
                    // Iteratively load previous history and retry until success or error.
                    start_ss = self.ss0 - 1;
                    continue;
                },
                Err(e) => return Err(Box::new(e)),
            };
            if let Some(commit) = c {
                trace!("Pushing merge commit: {} ({} changes)",
                        commit.statesum(), commit.num_changes());
                self.push_commit(commit)?;
            } else {
                return Err(Box::new(MergeError::NotSolved));
            }
        }
        Ok(())
    }
    
    /// Creates a `TwoWayMerge` for two given states (presumably tip states,
    /// but not required).
    /// 
    /// It is recommended to use `merge` instead unless you need control over merge order with more
    /// than two tips. In order to use this function, you'll need code like:
    /// 
    /// ```no_compile
    /// let commit = partition.merge_two(&tip1, &tip2)?
    ///         .solve_inline(&solver)
    ///         .make_commit(&mcm)
    ///         .expect("merge failed");
    /// partition.add_commit(commit)?;
    /// ```
    /// 
    /// Note that this function can fail with `MergeError::NoCommonAncestor` if not enough history
    /// is available. In this case you might try calling `part.load_all()?;` or
    /// `let ss0 = part.oldest_ss_loaded(); part.load_range(ss0 - 1, ss0);`, then retrying.
    pub fn merge_two(&self, tip1: &Sum, tip2: &Sum) -> Result<TwoWayMerge<C::Element>, MergeError> {
        let common = match self.latest_common_ancestor(tip1, tip2) {
            Ok(sum) => sum,
            Err(e) => return Err(e),
        };
        let s1 = self.states.get(tip1).ok_or(MergeError::NoState)?;
        let s2 = self.states.get(tip2).ok_or(MergeError::NoState)?;
        let s3 = self.states.get(&common).ok_or(MergeError::NoState)?;
        Ok(TwoWayMerge::new(s1, s2, s3))
    }
    
    // #0003: allow getting a reference to other states listing snapshots,
    // commits, getting non-current states and getting diffs.
    
    /// This adds a new commit to the list waiting to be written and updates
    /// the states and 'tips' stored internally by creating a new state from
    /// the commit.
    /// 
    /// Mutates the commit in the (very unlikely) case that its statesum
    /// clashes with another commit whose data is different.
    /// 
    /// Fails if the commit's parent is not found or the patch cannot be
    /// applied to it. In this case the commit is lost, but presumably either
    /// there was a programmatic error or memory corruption for this to occur.
    /// 
    /// Returns `Ok(true)` on success or `Ok(false)` if the commit matches an
    /// already known state.
    pub fn push_commit(&mut self, commit: Commit<C::Element>) -> Result<bool, PatchOp> {
        let state = {
            let parent = self.states.get(commit.first_parent())
                .ok_or(PatchOp::NoParent)?;
            PartState::from_state_commit(parent, &commit)?
        };  // end borrow on self (from parent)
        Ok(self.add_pair(commit, state))
    }
    
    /// Add a new state, assumed to be derived from an existing known state.
    /// 
    /// This creates a commit from the given state, converts the `MutPartState`
    /// to a `PartState` and adds it to the list of internal states, and
    /// updates the tip. The commit is added to the internal list
    /// waiting to be written to permanent storage (see `write()`).
    /// 
    /// Mutates the commit in the (very unlikely) case that its statesum
    /// clashes with another commit whose data is different.
    /// 
    /// Returns `Ok(true)` on success, or `Ok(false)` if the state matches its
    /// parent (i.e. hasn't been changed) or another already known state.
    pub fn push_state(&mut self, state: MutPartState<C::Element>) -> Result<bool, PatchOp> {
        let parent_sum = state.parent().clone();
        let new_state = PartState::from_mut(state, self.control.as_mcm_ref_mut());
        
        // #0019: Commit::from_diff compares old and new states and code be slow.
        // #0019: Instead, we could record each alteration as it happens.
        Ok(if let Some(commit) =
                Commit::from_diff(
                    self.states.get(&parent_sum).ok_or(PatchOp::NoParent)?,
                    &new_state)
            {
                self.add_pair(commit, new_state)
            } else {
                false
            }
        )
    }
    
    /// The number of commits waiting to be written to permanent storage by
    /// the `write(...)` function.
    pub fn unsaved_len(&self) -> usize {
        self.unsaved.len()
    }
    
    /// Require that a snapshot be written the next time `write_full` is called.
    /// (This property is not persisted across save/load.)
    pub fn require_snapshot(&mut self) {
        self.control.snapshot_policy().force_snapshot()
    }
    
    /// This will write all unsaved commits to a log on the disk. Does nothing
    /// if there are no queued changes.
    /// 
    /// Also see `write_full()`.
    /// 
    /// Returns true if any commits were written (i.e. unsaved commits
    /// were found). Returns false if nothing needed doing.
    /// 
    /// Note that writing to disk can fail. In this case it may be worth trying
    /// again.
    pub fn write_fast(&mut self) -> Result<bool> {
        // First step: write commits
        if self.unsaved.is_empty() {
            return Ok(false);
        }
        
        let header = self.make_header(FileType::CommitLog(0))?;
        
        // #0012: extend existing logs instead of always writing a new log file.
        let mut cl_num = self.control.io().ss_cl_len(self.ss1 - 1);
        debug!("Partition {}: writing {} commits to log {}-{}",
                self.name, self.unsaved.len(), self.ss1-1, cl_num);
        loop {
            if let Some(mut writer) = self.control.io_mut().new_ss_cl(self.ss1 - 1, cl_num)? {
                // Write a header since this is a new file:
                write_head(&header, &mut writer)?;
                start_log(&mut writer)?;
                
                // Now write commits:
                while !self.unsaved.is_empty() {
                    // We try to write the commit, then when successful remove it
                    // from the list of 'unsaved' commits.
                    write_commit(self.unsaved.front().unwrap(), &mut writer)?;
                    self.unsaved.pop_front().expect("pop_front");
                }
                
                return Ok(true);
            } else {
                // Log file already exists! So try another number.
                if cl_num > 1000_000 {
                    // We should give up eventually. When is arbitrary.
                    return Err(Box::new(OtherError::new("Commit log number too high")));
                }
                cl_num += 1;
            }
        }
    }
    
    /// This will write all unsaved commits to a log on the disk, then write a
    /// snapshot if needed.
    /// 
    /// Returns true if any commits were written (i.e. unsaved commits
    /// were found). Returns false if no unsaved commits were present. This
    /// value implies nothing about whether a snapshot was made.
    /// 
    /// Note that writing to disk can fail. In this case it may be worth trying
    /// again.
    pub fn write_full(&mut self) -> Result<bool> {
        let has_changes = self.write_fast()?;
        
        // Second step: maintenance operations
        if self.is_ready() && self.control.snapshot_policy().want_snapshot() {
            self.write_snapshot()?;
        }
        
        Ok(has_changes)
    }
    
    /// Write a new snapshot from the tip.
    /// 
    /// Normally you can just call `write_full()` and let the library figure out
    /// when to write a new snapshot, though you can also call this directly.
    /// 
    /// Does nothing when `tip()` fails (returning `Ok(())`).
    pub fn write_snapshot(&mut self) -> Result<()> {
        // fail early if not ready:
        let tip_key = self.tip_key()?.clone();
        let header = self.make_header(FileType::Snapshot(0))?;
        
        let mut ss_num = self.ss1;
        loop {
            
            // Try to get a writer for this snapshot number:
            if let Some(mut writer) = self.control.io_mut().new_ss(ss_num)? {
                debug!("Partition {}: writing snapshot {}: {}",
                    self.name, ss_num, tip_key);
                
                write_head(&header, &mut writer)?;
                write_snapshot(self.states.get(&tip_key).unwrap(), &mut writer)?;
            } else {
                // Snapshot file already exists! So try another number.
                if ss_num > 1000_000 {
                    // We should give up eventually. When is arbitrary.
                    return Err(Box::new(OtherError::new("Snapshot number too high")));
                }
                ss_num += 1;
                continue;
            }
            
            // After borrow on self.control expires:
            self.ss1 = ss_num + 1;
            self.control.snapshot_policy().reset();
            return Ok(())
        }
    }
}

// Internal support functions
impl<C: Control> Partition<C> {
    // Take self and two sums. Return a copy of a key to avoid lifetime issues.
    fn latest_common_ancestor(&self, k1: &Sum, k2: &Sum) -> Result<Sum, MergeError> {
        // #0019: there are multiple strategies here; we just find all
        // ancestors of one, then of the other. This simplifies lopic.
        let mut a1 = HashSet::new();
        
        let mut next = VecDeque::new();
        next.push_back(k1);
        while let Some(k) = next.pop_back() {
            if a1.contains(k) { continue; }
            a1.insert(k);
            if let Some(state) = self.states.get(k) {
                for p in state.parents() {
                    next.push_back(p);
                }
            }
        }
        
        // We track ancestors of k2 just to check we don't end up in a loop.
        let mut a2 = HashSet::new();
        
        // next is empty
        next.push_back(k2);
        while let Some(k) = next.pop_back() {
            if a2.contains(k) { continue; }
            a2.insert(k);
            if a1.contains(k) {
                return Ok(k.clone());
            }
            if let Some(state) = self.states.get(k) {
                for p in state.parents() {
                    next.push_back(p);
                }
            }
        }
        
        Err(MergeError::NoCommonAncestor)
    }
    
    /// Add a state, assuming that this isn't a new one (i.e. it's been loaded
    /// from a file and doesn't need to be saved).
    /// 
    /// Unlike add_pair, mutating isn't possible, so this just returns false
    /// if the sum is known without checking whether the state is different.
    /// 
    /// `n_edits` is the number of changes in a commit. Normally the state is
    /// created from a commit, and `n_edits = commit.num_changes()`; in the
    /// case the state is a loaded snapshot the "snapshot policy" should be
    /// reset afterwards, hence n_edits does not matter.
    /// 
    /// Returns true unless the given state's sum equals an existing one.
    fn add_state(&mut self, state: PartState<C::Element>, n_edits: usize) {
        trace!("Partition {}: add state {}", self.name, state.statesum());
        if self.states.contains(state.statesum()) {
            trace!("Partition {} already contains state {}", self.name, state.statesum());
            return;
        }
        
        for parent in state.parents() {
            // Remove from 'tips' if it happened to be there:
            self.tips.remove(parent);
            // Add to 'ancestors' if not in 'states':
            if !self.states.contains(parent) {
                self.ancestors.insert(parent.clone());
            }
        }
        // We know from above 'state' is not in 'self.states'; if it's not in
        // 'self.ancestors' either then it must be a tip:
        if !self.ancestors.contains(state.statesum()) {
            self.control.snapshot_policy().count(1, n_edits);
            self.tips.insert(state.statesum().clone());
        }
        // TODO: check that classification in state equals that of this partition?
        self.states.insert(state);
    }
    
    /// Creates a state from the commit and adds to self. Updates tip if this
    /// state is new.
    pub fn add_commit(&mut self, commit: Commit<C::Element>) -> Result<(), PatchOp> {
        if self.states.contains(commit.statesum()) { return Ok(()); }
        
        let state = {
            let parent = self.states.get(commit.first_parent())
                .ok_or(PatchOp::NoParent)?;
            PartState::from_state_commit(parent, &commit)?
        };  // end borrow on self (from parent)
        self.add_state(state, commit.num_changes());
        Ok(())
    }
    
    /// Add a paired commit and state, asserting that the checksums match and
    /// the parent state is present. Also add to the queue awaiting `write()`.
    /// 
    /// If an element with the states's statesum already exists and differs
    /// from the state passed, the state and commit passed will be mutated to
    /// achieve a unique statesum.
    /// 
    /// Returns true unless the given state (including metadata) equals a
    /// stored one (in which case nothing happens and false is returned).
    fn add_pair(&mut self, mut commit: Commit<C::Element>, mut state: PartState<C::Element>) -> bool {
        trace!("Partition {}: add commit {}", self.name, commit.statesum());
        assert_eq!(commit.parents(), state.parents());
        assert_eq!(commit.statesum(), state.statesum());
        assert!(self.states.contains(commit.first_parent()));
        
        while let Some(old_state) = self.states.get(state.statesum()) {
            if state == *old_state {
                trace!("Partition {} already contains commit {}", self.name, commit.statesum());
                return false;
            } else {
                commit.mutate_meta(state.mutate_meta());
                trace!("Partition {}: mutated commit to {}", self.name, commit.statesum());
            }
        }
        
        self.add_state(state, commit.num_changes());
        self.unsaved.push_back(commit);
        true
    }
}


/// Wrapper around underlying iterator structure
pub struct TipIter<'a> {
    iter: hs::Iter<'a, Sum>
}
impl<'a> Clone for TipIter<'a> {
    fn clone(&self) -> TipIter<'a> {
        TipIter { iter: self.iter.clone() }
    }
}
impl<'a> Iterator for TipIter<'a> {
    type Item = &'a Sum;
    fn next(&mut self) -> Option<&'a Sum> {
        self.iter.next()
    }
}
impl<'a> ExactSizeIterator for TipIter<'a> {
    fn len(&self) -> usize {
        self.iter.len()
    }
}

/// Wrapper around a `PartState<E>`. Dereferences to this type.
pub struct StateItem<'a, E: Element+'a> {
    state: &'a PartState<E>,
    tips: &'a HashSet<Sum>,
}
impl<'a, E: Element+'a> StateItem<'a, E> {
    /// Returns true if and only if this state is a tip state (i.e. is not the
    /// parent of any other state).
    /// 
    /// There is exactly one tip state unless a merge is required or no states
    /// are loaded.
    pub fn is_tip(&self) -> bool {
        self.tips.contains(self.state.statesum())
    }
}
impl<'a, E: Element+'a> Deref for StateItem<'a, E> {
    type Target = PartState<E>;
    fn deref(&self) -> &Self::Target {
        self.state
    }
}

/// Iterator over a partition's (historical or current) states
pub struct StateIter<'a, E: Element+'a> {
    iter: Iter<'a, PartState<E>, Sum, PartStateSumComparator>,
    tips: &'a HashSet<Sum>,
}
impl<'a, E: Element+'a> Iterator for StateIter<'a, E> {
    type Item = StateItem<'a, E>;
    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|item|
            StateItem {
                state: item,
                tips: self.tips,
            }
        )
    }
    fn size_hint(&self) -> (usize, Option<usize>) { self.iter.size_hint() }
}


#[cfg(test)]
mod tests {
    use super::*;
    use elt::EltId;
    use commit::{Commit, MakeCommitMeta};
    use control::DefaultControl;
    use io::DummyRepoIO;
    use state::*;
    
    struct MCM;
    impl MakeCommitMeta for MCM {}
    
    #[test]
    fn commit_creation_and_replay(){
        let mut queue = vec![];
        let mut mcm = MCM;
        
        let insert = |state: &mut MutPartState<_>, num, string: &str| -> Result<_, _> {
            state.insert(EltId::from(num), string.to_string())
        };
        
        let mut state = PartState::new(&mut mcm).clone_mut();
        insert(&mut state, 1, "one").unwrap();
        insert(&mut state, 2, "two").unwrap();
        let state_a = PartState::from_mut(state, &mut mcm);
        
        let mut state = state_a.clone_mut();
        insert(&mut state, 3, "three").unwrap();
        insert(&mut state, 4, "four").unwrap();
        insert(&mut state, 5, "five").unwrap();
        let state_b = PartState::from_mut(state, &mut mcm);
        let commit = Commit::from_diff(&state_a, &state_b).unwrap();
        queue.push(commit);
        
        let mut state = state_b.clone_mut();
        insert(&mut state, 6, "six").unwrap();
        insert(&mut state, 7, "seven").unwrap();
        state.remove(EltId::from(4)).unwrap();
        state.replace(EltId::from(3), "half six".to_string()).unwrap();
        let state_c = PartState::from_mut(state, &mut mcm);
        let commit = Commit::from_diff(&state_b, &state_c).unwrap();
        queue.push(commit);
        
        let mut state = state_c.clone_mut();
        insert(&mut state, 8, "eight").unwrap();
        insert(&mut state, 4, "half eight").unwrap();
        let state_d = PartState::from_mut(state, &mut mcm);
        let commit = Commit::from_diff(&state_c, &state_d).unwrap();
        queue.push(commit);
        
        let control = DefaultControl::<String, _>::new(DummyRepoIO::new());
        let mut part = Partition::create(control, "replay part").unwrap();
        part.add_state(state_a, 0);
        for commit in queue {
            part.push_commit(commit).unwrap();
        }
        
        assert_eq!(part.tips.len(), 1);
        let replayed_state = part.tip().unwrap();
        assert_eq!(*replayed_state, state_d);
    }
    
    #[test]
    fn on_new_partition() {
        let control = DefaultControl::<String, _>::new(DummyRepoIO::new());
        let mut part = Partition::create(control, "on_new_partition")
                .expect("partition creation");
        assert_eq!(part.tips.len(), 1);
        
        let state = part.tip().expect("getting tip").clone_mut();
        assert_eq!(part.push_state(state).expect("committing"), false);
        
        let mut state = part.tip().expect("getting tip").clone_mut();
        assert!(!state.any_avail());
        
        let elt1 = "This is element one.".to_string();
        let elt2 = "Element two data.".to_string();
        let e1id = state.insert_new(elt1).expect("inserting elt");
        let e2id = state.insert_new(elt2).expect("inserting elt");
        
        assert_eq!(part.push_state(state).expect("comitting"), true);
        assert_eq!(part.unsaved.len(), 1);
        assert_eq!(part.states.len(), 2);
        let key = part.tip().expect("tip").statesum().clone();
        {
            let state = part.state(&key).expect("getting state by key");
            assert!(state.is_avail(e1id));
            assert_eq!(state.get(e2id), Ok(&"Element two data.".to_string()));
        }   // `state` goes out of scope
        assert_eq!(part.tips.len(), 1);
        let state = part.tip().expect("getting tip").clone_mut();
        assert_eq!(*state.parent(), key);
        
        assert_eq!(part.push_state(state).expect("committing"), false);
    }
}
