use std::time::Duration;

use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use serde_json::json;

use crate::category::{Category, CategoryHandle, CategoryPairHandle};
use crate::category_color::CategoryColor;
use crate::cpu_delta::CpuDelta;
use crate::fast_hash_map::FastHashMap;
use crate::frame::Frame;
use crate::frame_table::{InternalFrame, InternalFrameLocation};
use crate::global_lib_table::GlobalLibTable;
use crate::library_info::LibraryInfo;
use crate::libs_with_ranges::LibsWithRanges;
use crate::process::{Process, ThreadHandle};
use crate::reference_timestamp::ReferenceTimestamp;
use crate::string_table::{GlobalStringIndex, GlobalStringTable};
use crate::thread::{ProcessHandle, Thread};
use crate::{MarkerSchema, MarkerTiming, ProfilerMarker, Timestamp};

/// The sampling interval used during profile recording.
///
/// This doesn't have to match the actual delta between sample timestamps.
/// It just describes the intended interval.
///
/// For profiles without sampling data, this can be set to a meaningless
/// dummy value.
#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Hash)]
pub struct SamplingInterval {
    nanos: u64,
}

impl SamplingInterval {
    /// Create a sampling interval from a sampling frequency in Hz.
    ///
    /// Panics on zero or negative values.
    pub fn from_hz(samples_per_second: f32) -> Self {
        assert!(samples_per_second > 0.0);
        let nanos = (1_000_000_000.0 / samples_per_second) as u64;
        Self::from_nanos(nanos)
    }

    /// Create a sampling interval from a value in milliseconds.
    pub fn from_millis(millis: u64) -> Self {
        Self::from_nanos(millis * 1_000_000)
    }

    /// Create a sampling interval from a value in nanoseconds
    pub fn from_nanos(nanos: u64) -> Self {
        Self { nanos }
    }

    /// Convert the interval to nanoseconds.
    pub fn nanos(&self) -> u64 {
        self.nanos
    }

    /// Convert the interval to float seconds.
    pub fn as_secs_f64(&self) -> f64 {
        self.nanos as f64 / 1_000_000_000.0
    }
}

impl From<Duration> for SamplingInterval {
    fn from(duration: Duration) -> Self {
        Self::from_nanos(duration.as_nanos() as u64)
    }
}

/// A handle for an interned string, returned from [`Profile::intern_string`].
#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Hash)]
pub struct StringHandle(GlobalStringIndex);

/// Stores the profile data and can be serialized as JSON, via [`serde::Serialize`].
///
/// The profile data is organized into a list of processes with threads.
/// Each thread has its own samples and markers.
///
/// ```
/// use fxprof_processed_profile::{Profile, CategoryHandle, CpuDelta, Frame, SamplingInterval, Timestamp};
/// use std::time::SystemTime;
///
/// # fn write_profile(output_file: std::fs::File) -> Result<(), Box<dyn std::error::Error>> {
/// let mut profile = Profile::new("My app", SystemTime::now().into(), SamplingInterval::from_millis(1));
/// let process = profile.add_process("App process", 54132, Timestamp::from_millis_since_reference(0.0));
/// let thread = profile.add_thread(process, 54132000, Timestamp::from_millis_since_reference(0.0), true);
/// profile.set_thread_name(thread, "Main thread");
/// let stack = vec![
///     (Frame::Label(profile.intern_string("Root node")), CategoryHandle::OTHER.into()),
///     (Frame::Label(profile.intern_string("First callee")), CategoryHandle::OTHER.into())
/// ];
/// profile.add_sample(thread, Timestamp::from_millis_since_reference(0.0), stack.into_iter(), CpuDelta::ZERO, 1);
///
/// let writer = std::io::BufWriter::new(output_file);
/// serde_json::to_writer(writer, &profile)?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Profile {
    pub(crate) product: String,
    pub(crate) interval: SamplingInterval,
    pub(crate) global_libs: GlobalLibTable,
    pub(crate) kernel_libs: LibsWithRanges,
    pub(crate) categories: Vec<Category>, // append-only for stable CategoryHandles
    pub(crate) processes: Vec<Process>,   // append-only for stable ProcessHandles
    pub(crate) threads: Vec<Thread>,      // append-only for stable ThreadHandles
    pub(crate) reference_timestamp: ReferenceTimestamp,
    pub(crate) string_table: GlobalStringTable,
    pub(crate) marker_schemas: FastHashMap<&'static str, MarkerSchema>,
}

impl Profile {
    /// Create a new profile.
    ///
    /// The `product` is the name of the main application which was profiled.
    /// The `reference_timestamp` is some arbitrary absolute timestamp which all
    /// other timestamps in the profile data are relative to. The `interval` is the intended
    /// time delta between samples.
    pub fn new(
        product: &str,
        reference_timestamp: ReferenceTimestamp,
        interval: SamplingInterval,
    ) -> Self {
        Profile {
            interval,
            product: product.to_string(),
            threads: Vec::new(),
            global_libs: GlobalLibTable::new(),
            kernel_libs: LibsWithRanges::new(),
            reference_timestamp,
            processes: Vec::new(),
            string_table: GlobalStringTable::new(),
            marker_schemas: FastHashMap::default(),
            categories: vec![Category {
                name: "Other".to_string(),
                color: CategoryColor::Grey,
                subcategories: Vec::new(),
            }],
        }
    }

    /// Change the declared sampling interval.
    pub fn set_interval(&mut self, interval: SamplingInterval) {
        self.interval = interval;
    }

    /// Change the reference timestamp.
    pub fn set_reference_timestamp(&mut self, reference_timestamp: ReferenceTimestamp) {
        self.reference_timestamp = reference_timestamp;
    }

    /// Change the product name.
    pub fn set_product(&mut self, product: &str) {
        self.product = product.to_string();
    }

    /// Add a category and return its handle.
    ///
    /// Categories are used for stack frames and markers, as part of a "category pair".
    pub fn add_category(&mut self, name: &str, color: CategoryColor) -> CategoryHandle {
        let handle = CategoryHandle(self.categories.len() as u16);
        self.categories.push(Category {
            name: name.to_string(),
            color,
            subcategories: Vec::new(),
        });
        handle
    }

    /// Add a subcategory for a category, and return the "category pair" handle.
    pub fn add_subcategory(&mut self, category: CategoryHandle, name: &str) -> CategoryPairHandle {
        let subcategory = self.categories[category.0 as usize].add_subcategory(name.into());
        CategoryPairHandle(category, Some(subcategory))
    }

    /// Add an empty process. The name, pid and start time can be changed afterwards,
    /// but they are required here because they have to be present in the profile JSON.
    pub fn add_process(&mut self, name: &str, pid: u32, start_time: Timestamp) -> ProcessHandle {
        let handle = ProcessHandle(self.processes.len());
        self.processes.push(Process::new(name, pid, start_time));
        handle
    }

    /// Change the start time of a process.
    pub fn set_process_start_time(&mut self, process: ProcessHandle, start_time: Timestamp) {
        self.processes[process.0].set_start_time(start_time);
    }

    /// Set the end time of a process.
    pub fn set_process_end_time(&mut self, process: ProcessHandle, end_time: Timestamp) {
        self.processes[process.0].set_end_time(end_time);
    }

    /// Change the name of a process.
    pub fn set_process_name(&mut self, process: ProcessHandle, name: &str) {
        self.processes[process.0].set_name(name);
    }

    /// Add a library which is loaded into a process. This allows symbolication of native
    /// stacks once the profile is opened in the Firefox Profiler.
    ///
    /// Each library covers an address range in the virtual memory of a process. Future calls
    /// to [`Profile::add_sample`] with native frames resolve the frame's code address with
    /// respect to the currently loaded kernel and process libraries.
    pub fn add_lib(&mut self, process: ProcessHandle, library: LibraryInfo) {
        self.processes[process.0].add_lib(library);
    }

    /// Mark the library at the specified base address in the specified process as unloaded,
    /// so that future calls to [`Profile::add_sample`] know about the unloading.
    pub fn unload_lib(&mut self, process: ProcessHandle, base_address: u64) {
        self.processes[process.0].unload_lib(base_address);
    }

    /// Add a kernel library. This allows symbolication of kernel stacks once the profile is
    /// opened in the Firefox Profiler. Kernel libraries are global and not tied to a process.
    ///
    /// Each kernel library covers an address range in the kernel address space, which is
    /// global across all processes. Future calls to [`Profile::add_sample`] with native
    /// frames resolve the frame's code address with respect to the currently loaded kernel
    /// and process libraries.
    pub fn add_kernel_lib(&mut self, library: LibraryInfo) {
        self.kernel_libs.add_lib(library);
    }

    /// Mark the kernel library at the specified base address in the specified process as
    /// unloaded, so that future calls to [`Profile::add_sample`] know about the unloading.
    pub fn unload_kernel_lib(&mut self, base_address: u64) {
        self.kernel_libs.unload_lib(base_address);
    }

    /// Add an empty thread to the specified process.
    pub fn add_thread(
        &mut self,
        process: ProcessHandle,
        tid: u32,
        start_time: Timestamp,
        is_main: bool,
    ) -> ThreadHandle {
        let handle = ThreadHandle(self.threads.len());
        self.threads
            .push(Thread::new(process, tid, start_time, is_main));
        self.processes[process.0].add_thread(handle);
        handle
    }

    /// Change the name of a thread.
    pub fn set_thread_name(&mut self, thread: ThreadHandle, name: &str) {
        self.threads[thread.0].set_name(name);
    }

    /// Change the start time of a thread.
    pub fn set_thread_start_time(&mut self, thread: ThreadHandle, start_time: Timestamp) {
        self.threads[thread.0].set_start_time(start_time);
    }

    /// Set the end time of a thread.
    pub fn set_thread_end_time(&mut self, thread: ThreadHandle, end_time: Timestamp) {
        self.threads[thread.0].set_end_time(end_time);
    }

    /// Turn the string into in a [`StringHandle`], for use in [`Frame::Label`].
    pub fn intern_string(&mut self, s: &str) -> StringHandle {
        StringHandle(self.string_table.index_for_string(s))
    }

    /// Add a sample to the given thread.
    ///
    /// The sample has a timestamp, a stack, a CPU delta, and a weight.
    ///
    /// The stack frames are supplied as an iterator. Every frame has an associated
    /// category pair.
    ///
    /// The CPU delta is the amount of CPU time that the CPU was busy with work for this
    /// thread since the previous sample. It should always be less than or equal the
    /// time delta between the sample timestamps.
    ///
    /// The weight affects the sample's stack's score in the call tree. You usually set
    /// this to 1. You can use weights greater than one if you want to combine multiple
    /// adjacent samples with the same stack into one sample, to save space. However,
    /// this discards any CPU deltas between the adjacent samples, so it's only really
    /// useful if no CPU time has occurred between the samples, and for that use case the
    /// [`Profile::add_sample_same_stack_zero_cpu`] method should be preferred.
    ///
    /// You can can also set the weight to something negative, such as -1, to create a
    /// "diff profile". For example, if you have partitioned your samples into "before"
    /// and "after" groups, you can use -1 for all "before" samples and 1 for all "after"
    /// samples, and the call tree will show you which stacks occur more frequently in
    /// the "after" part of the profile, by sorting those stacks to the top.
    pub fn add_sample(
        &mut self,
        thread: ThreadHandle,
        timestamp: Timestamp,
        frames: impl Iterator<Item = (Frame, CategoryPairHandle)>,
        cpu_delta: CpuDelta,
        weight: i32,
    ) {
        let stack_index = self.stack_index_for_frames(thread, frames);
        self.threads[thread.0].add_sample(timestamp, stack_index, cpu_delta, weight);
    }

    /// Add a sample with a CPU delta of zero. Internally, multiple consecutive
    /// samples with a delta of zero will be combined into one sample with an accumulated
    /// weight.
    pub fn add_sample_same_stack_zero_cpu(
        &mut self,
        thread: ThreadHandle,
        timestamp: Timestamp,
        weight: i32,
    ) {
        self.threads[thread.0].add_sample_same_stack_zero_cpu(timestamp, weight);
    }

    /// Add a marker to the given thread.
    pub fn add_marker<T: ProfilerMarker>(
        &mut self,
        thread: ThreadHandle,
        name: &str,
        marker: T,
        timing: MarkerTiming,
    ) {
        self.marker_schemas
            .entry(T::MARKER_TYPE_NAME)
            .or_insert_with(T::schema);
        self.threads[thread.0].add_marker(name, marker, timing);
    }

    // frames is ordered from caller to callee, i.e. root function first, pc last
    fn stack_index_for_frames(
        &mut self,
        thread: ThreadHandle,
        frames: impl Iterator<Item = (Frame, CategoryPairHandle)>,
    ) -> Option<usize> {
        let thread = &mut self.threads[thread.0];
        let process = &mut self.processes[thread.process().0];
        let mut prefix = None;
        for (frame, category_pair) in frames {
            let location = match frame {
                Frame::InstructionPointer(ip) => {
                    process.convert_address(&mut self.global_libs, &mut self.kernel_libs, ip)
                }
                Frame::ReturnAddress(ra) => process.convert_address(
                    &mut self.global_libs,
                    &mut self.kernel_libs,
                    ra.saturating_sub(1),
                ),
                Frame::Label(string_index) => {
                    let thread_string_index =
                        thread.convert_string_index(&self.string_table, string_index.0);
                    InternalFrameLocation::Label(thread_string_index)
                }
            };
            let internal_frame = InternalFrame {
                location,
                category_pair,
            };
            let frame_index = thread.frame_index_for_frame(internal_frame, &self.global_libs);
            prefix = Some(thread.stack_index_for_stack(prefix, frame_index, category_pair));
        }
        prefix
    }
}

impl Serialize for Profile {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("meta", &SerializableProfileMeta(self))?;
        map.serialize_entry("libs", &self.global_libs)?;
        map.serialize_entry("threads", &SerializableProfileThreadsProperty(self))?;
        map.serialize_entry("pages", &[] as &[()])?;
        map.serialize_entry("profilerOverhead", &[] as &[()])?;
        map.serialize_entry("counters", &[] as &[()])?;
        map.end()
    }
}

struct SerializableProfileMeta<'a>(&'a Profile);

impl<'a> Serialize for SerializableProfileMeta<'a> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("categories", &self.0.categories)?;
        map.serialize_entry("debug", &false)?;
        map.serialize_entry(
            "extensions",
            &json!({
                "length": 0,
                "baseURL": [],
                "id": [],
                "name": [],
            }),
        )?;
        map.serialize_entry("interval", &(self.0.interval.as_secs_f64() * 1000.0))?;
        map.serialize_entry("preprocessedProfileVersion", &44)?;
        map.serialize_entry("processType", &0)?;
        map.serialize_entry("product", &self.0.product)?;
        map.serialize_entry(
            "sampleUnits",
            &json!({
                "time": "ms",
                "eventDelay": "ms",
                "threadCPUDelta": "µs",
            }),
        )?;
        map.serialize_entry("startTime", &self.0.reference_timestamp)?;
        map.serialize_entry("symbolicated", &false)?;
        map.serialize_entry("pausedRanges", &[] as &[()])?;
        map.serialize_entry("version", &24)?;
        map.serialize_entry("usesOnlyOneStackType", &true)?;
        map.serialize_entry("doesNotUseFrameImplementation", &true)?;
        map.serialize_entry("sourceCodeIsNotOnSearchfox", &true)?;

        let mut marker_schemas: Vec<MarkerSchema> =
            self.0.marker_schemas.values().cloned().collect();
        marker_schemas.sort_by_key(|schema| schema.type_name);
        map.serialize_entry("markerSchema", &marker_schemas)?;

        map.end()
    }
}

struct SerializableProfileThreadsProperty<'a>(&'a Profile);

impl<'a> Serialize for SerializableProfileThreadsProperty<'a> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // The processed profile format has all threads from all processes in a flattened threads list.
        // Each thread duplicates some information about its process, which allows the Firefox Profiler
        // UI to group threads from the same process.

        let mut seq = serializer.serialize_seq(Some(self.0.threads.len()))?;

        let mut sorted_processes: Vec<_> = (0..self.0.processes.len()).map(ProcessHandle).collect();
        sorted_processes.sort_by(|a_handle, b_handle| {
            let a = &self.0.processes[a_handle.0];
            let b = &self.0.processes[b_handle.0];
            a.cmp_for_json_order(b)
        });

        for process in sorted_processes {
            let mut sorted_threads = self.0.processes[process.0].threads();
            sorted_threads.sort_by(|a_handle, b_handle| {
                let a = &self.0.threads[a_handle.0];
                let b = &self.0.threads[b_handle.0];
                a.cmp_for_json_order(b)
            });

            for thread in sorted_threads {
                let categories = &self.0.categories;
                let thread = &self.0.threads[thread.0];
                let process = &self.0.processes[thread.process().0];
                seq.serialize_element(&SerializableProfileThread(process, thread, categories))?;
            }
        }

        seq.end()
    }
}

struct SerializableProfileThread<'a>(&'a Process, &'a Thread, &'a [Category]);

impl<'a> Serialize for SerializableProfileThread<'a> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let SerializableProfileThread(process, thread, categories) = self;
        let process_start_time = process.start_time();
        let process_end_time = process.end_time();
        let process_name = process.name();
        let pid = process.pid();
        thread.serialize_with(
            serializer,
            categories,
            process_start_time,
            process_end_time,
            process_name,
            pid,
        )
    }
}
