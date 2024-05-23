use std::{fs::File, io::{BufRead, BufReader, Lines}, path::Path};

use fxprof_processed_profile::{CategoryHandle, MarkerTiming, Profile, ThreadHandle, Timestamp};

use crate::shared::{process_sample_data::SimpleMarker, timestamp_converter::TimestampConverter};

pub struct Marker {
    tid: i32,
    start: Timestamp,
    end: Timestamp,
    name: String,
    value: String
}

pub struct MoreMarkers {
    markers: Vec<Marker>,
}

fn process_marker_span_line(
    line: &str,
    timestamp_converter: &TimestampConverter,
) -> Option<Marker> {
    println!("{}", line);
    let mut split = line.splitn(5, ' ');
    let tid = split.next().unwrap();
    let name = split.next().unwrap().to_owned();
    let start_time = split.next().unwrap();
    let end_time = split.next().unwrap();
    let value = split.next().unwrap().to_owned();
    //if name.is_empty() {
    //    return panic!("flam");
    //}
    let tid = tid.parse::<i32>().ok()?;
    let start = timestamp_converter.convert_time(start_time.parse::<u64>().ok()?);
    let end = timestamp_converter.convert_time(end_time.parse::<u64>().ok()?);
    Some(Marker {tid, start, end, name, value})
}

pub struct MarkerFile {
    lines: Lines<BufReader<File>>,
    timestamp_converter: TimestampConverter,
}

impl MarkerFile {
    pub fn parse(file: File, timestamp_converter: TimestampConverter) -> Self {
        Self {
            lines: BufReader::new(file).lines(),
            timestamp_converter,
        }
    }
}

impl Iterator for MarkerFile {
    type Item = Marker;

    fn next(&mut self) -> Option<Self::Item> {
        let line = self.lines.next()?.ok()?;
        process_marker_span_line(&line, &self.timestamp_converter)
    }
}

impl MoreMarkers {
    pub fn new() -> Self {
        Self {
            markers: Vec::new(),
        }
    }

    pub fn read_from_file(&mut self, path: &Path, timestamp_converter: TimestampConverter) -> Option<()> {
        let f = File::open(path).ok()?;
        let marker_file = MarkerFile::parse(f, timestamp_converter);
        self.markers.extend(marker_file);
        eprintln!("Have {} markers from this file: {}", self.markers.len(), path.to_string_lossy());
        Some(())
    }

    pub fn add_thread_markers(&self, profile: &mut Profile, tid: i32, thread_handle: ThreadHandle) {
        let mut c = 0;
        for marker in self.markers.iter().filter(|m| m.tid == tid) {
            profile.add_marker(thread_handle, CategoryHandle::OTHER, &marker.name, SimpleMarker(marker.value.clone()), MarkerTiming::Interval(marker.start, marker.end));
            c += 1;
        }
        if c != 0 {
            eprintln!("Added {c} markers for tid {tid}");
        }
    }

    // pub fn markers_for_tid(&self, tid: i32) -> Option<Vec<MarkerSpan>> {
    //     let markers: Vec<_> = self.markers.iter().filter_map(|m| {
    //         if m.0 != tid {
    //             return None;
    //         }
    //         Some(MarkerSpan {
    //             start_time: m.1,
    //             end_time: m.2,
    //             name: m.3.clone()
    //         })

    //     }).collect();

    //     if markers.is_empty() {
    //         None
    //     } else {
    //         Some(markers)
    //     }
    // }
}
