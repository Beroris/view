//! Trimming a video: keeping the part between a start and an end, splitting it into parts and
//! deleting some, and silencing some parts' sound or all of it; saved over the original or as a
//! copy.
//!
//! The picture is copied across as it is, never decoded or encoded again, so it's quick and
//! loses nothing. The catch is that a piece of video can only start on a key frame, so the start,
//! and the split after a deleted part, move to the key frame before (a little less goes). The
//! sound is copied as it is too, unless parts of it are silenced: then it's decoded, the parts
//! zeroed, and encoded again as AAC. Media Foundation reads the file and writes the MP4. Drawing
//! is in `screens.rs`; the window's side is in `tools.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::GUID;

/// The shortest a trimmed video can be, in seconds.
pub const SHORTEST: f64 = 0.5;
/// The closest two splits can be, in seconds.
pub const CLOSEST: f64 = 0.1;
/// The most splits there can be.
pub const MOST_SPLITS: usize = 99;

/// One of the parts the splits make: whether it's deleted, and whether its sound is.
#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub struct Part {
    pub deleted: bool,
    pub silent: bool,
}

/// Everything Undo puts back.
#[derive(Clone, PartialEq, Debug)]
pub struct State {
    start: f64,
    end: f64,
    splits: Vec<f64>,
    parts: Vec<Part>,
    mute: bool,
}

/// A video being trimmed.
pub struct Trim {
    pub path: PathBuf,
    /// Its length, in seconds.
    pub duration: f64,
    /// What's kept, in seconds, before parts are deleted.
    pub start: f64,
    pub end: f64,
    /// Where it's split, in order. They make one more part than there are of them.
    pub splits: Vec<f64>,
    pub parts: Vec<Part>,
    /// The part picked, to delete or silence.
    pub selected: Option<usize>,
    /// Whether all the sound is left out.
    pub mute: bool,
    /// Whether it has sound that can be kept.
    pub sound: bool,
    /// While saving: how far it's got, in thousandths.
    pub saving: Option<Arc<AtomicU32>>,
    /// The preview's sound is off, for a silenced part.
    pub hushed: bool,
    /// How it was before each change, the last last; and changes undone, to do again.
    undo: Vec<State>,
    redo: Vec<State>,
}

impl Trim {
    pub fn new(path: &Path, probe: Probe) -> Trim {
        Trim { path: path.to_path_buf(), duration: probe.duration, start: 0.0, end: probe.duration, splits: Vec::new(), parts: vec![Part::default()], selected: None, mute: false, sound: probe.sound, saving: None, hushed: false, undo: Vec::new(), redo: Vec::new() }
    }

    fn state(&self) -> State {
        State { start: self.start, end: self.end, splits: self.splits.clone(), parts: self.parts.clone(), mute: self.mute }
    }

    fn apply(&mut self, s: State) {
        (self.start, self.end, self.splits, self.parts, self.mute) = (s.start, s.end, s.splits, s.parts, s.mute);
        self.selected = None;
    }

    /// Note how it is, before a change, for Undo.
    pub fn remember(&mut self) {
        let now = self.state();
        if self.undo.last() != Some(&now) {
            self.undo.push(now);
            self.redo.clear();
        }
    }

    /// Put back how it was before the last change. Notes that turned out to change nothing are
    /// passed over.
    pub fn undo(&mut self) -> bool {
        let now = self.state();
        while let Some(before) = self.undo.pop() {
            if before != now {
                self.redo.push(now);
                self.apply(before);
                return true;
            }
        }
        false
    }

    /// Do again the last change undone.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else { return false };
        self.undo.push(self.state());
        self.apply(next);
        true
    }

    pub fn can_undo(&self) -> bool {
        let now = self.state();
        self.undo.iter().any(|s| *s != now)
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    // ---- The bar: the parts not deleted, end to end ----

    /// The parts left, in order: where each starts and ends in the video.
    pub fn shown(&self) -> Vec<(f64, f64)> {
        (0..self.parts.len()).filter(|&i| !self.parts[i].deleted).map(|i| self.bounds(i)).collect()
    }

    /// How long the parts left are, end to end.
    pub fn length(&self) -> f64 {
        self.shown().iter().map(|(a, b)| b - a).sum::<f64>().max(0.001)
    }

    /// Where a time in the video is along the bar. A time in a deleted part goes where it was.
    pub fn to_line(&self, t: f64) -> f64 {
        let mut x = 0.0;
        for (a, b) in self.shown() {
            if t < a {
                return x;
            }
            if t <= b {
                return x + (t - a);
            }
            x += b - a;
        }
        x
    }

    /// The time in the video at a point along the bar. Where two parts meet, the later one.
    pub fn from_line(&self, x: f64) -> f64 {
        let shown = self.shown();
        let mut at = 0.0;
        for (k, &(a, b)) in shown.iter().enumerate() {
            if x < at + (b - a) || k + 1 == shown.len() {
                return (a + (x - at)).clamp(a, b);
            }
            at += b - a;
        }
        self.duration
    }

    /// Whether the video runs on from part `i` straight into the next part left, so the split
    /// between them can be moved.
    pub fn split_shown(&self, i: usize) -> bool {
        i + 1 < self.parts.len() && !self.parts[i].deleted && !self.parts[i + 1].deleted
    }

    /// Whether anything has been changed, so leaving would lose it.
    pub fn has_changes(&self) -> bool {
        self.start > 0.01 || self.end < self.duration - 0.01 || (self.mute && self.sound) || self.parts.iter().any(|p| p.deleted || (p.silent && self.sound))
    }

    pub fn reset(&mut self) {
        self.start = 0.0;
        self.end = self.duration;
        self.mute = false;
        self.splits.clear();
        self.parts = vec![Part::default()];
        self.selected = None;
    }

    /// Delete the part picked, unless it's the last one left. Returns it.
    pub fn delete_selected(&mut self) -> Option<usize> {
        let i = self.selected?;
        if self.parts.get(i)?.deleted || self.parts.iter().filter(|p| !p.deleted).count() < 2 {
            return None;
        }
        self.parts[i].deleted = true;
        self.selected = None;
        Some(i)
    }

    /// Move the start, keeping it before the end.
    pub fn set_start(&mut self, t: f64) {
        self.start = t.clamp(0.0, (self.end - SHORTEST).max(0.0));
    }

    /// Move the end, keeping it after the start.
    pub fn set_end(&mut self, t: f64) {
        self.end = t.clamp((self.start + SHORTEST).min(self.duration), self.duration);
    }

    /// Where part `i` starts and ends.
    pub fn bounds(&self, i: usize) -> (f64, f64) {
        let from = if i == 0 { 0.0 } else { self.splits[i - 1] };
        let to = self.splits.get(i).copied().unwrap_or(self.duration);
        (from, to)
    }

    /// The part a time is in.
    pub fn part_at(&self, t: f64) -> usize {
        self.splits.iter().filter(|&&s| s <= t).count()
    }

    /// Split the part a time is in into two, both as it was. Returns which split it is, or
    /// nothing if it's too close to another split or an end.
    pub fn split(&mut self, t: f64) -> Option<usize> {
        if self.splits.len() >= MOST_SPLITS || t < CLOSEST || t > self.duration - CLOSEST || self.splits.iter().any(|&s| (s - t).abs() < CLOSEST) {
            return None;
        }
        let i = self.part_at(t);
        self.splits.insert(i, t);
        self.parts.insert(i, self.parts[i]);
        // The part picked keeps being picked: the first half of it, if it was the one split.
        if let Some(sel) = self.selected.filter(|&s| s > i) {
            self.selected = Some(sel + 1);
        }
        Some(i)
    }

    /// Move a split, keeping it between its neighbours.
    pub fn set_split(&mut self, i: usize, t: f64) {
        let (lo, hi) = (if i == 0 { 0.0 } else { self.splits[i - 1] }, self.splits.get(i + 1).copied().unwrap_or(self.duration));
        if let Some(s) = self.splits.get_mut(i) {
            *s = t.clamp(lo + CLOSEST, (hi - CLOSEST).max(lo + CLOSEST));
        }
    }

    /// The video after a deleted part has to start on a key frame: the split there moves back to
    /// one (or on, if going back would leave nothing deleted).
    pub fn snap_split(&mut self, i: usize) -> Option<f64> {
        let s = *self.splits.get(i)?;
        if !self.parts[i].deleted || self.parts[i + 1].deleted {
            return None;
        }
        let (from, _) = self.bounds(i);
        let next = self.splits.get(i + 1).copied().unwrap_or(self.duration);
        let k = cut_end(&self.path, from, s)?;
        if k <= from + CLOSEST / 2.0 || k >= next - CLOSEST {
            return None;
        }
        self.splits[i] = k;
        Some(k)
    }

    /// Silence the part picked, or give it its sound back.
    pub fn toggle_silent(&mut self) -> Option<bool> {
        let p = self.parts.get_mut(self.selected?)?;
        p.silent = !p.silent;
        Some(p.silent)
    }

    /// The pieces of video kept, in order: from the start to the end, less the parts deleted.
    pub fn pieces(&self) -> Vec<(f64, f64)> {
        let mut pieces: Vec<(f64, f64)> = Vec::new();
        for (i, p) in self.parts.iter().enumerate() {
            let (a, b) = self.bounds(i);
            let (a, b) = (a.max(self.start), b.min(self.end));
            if p.deleted || b - a < 0.02 {
                continue;
            }
            match pieces.last_mut() {
                Some(last) if (last.1 - a).abs() < 1e-9 => last.1 = b,
                _ => pieces.push((a, b)),
            }
        }
        pieces
    }

    /// How long the saved video will be.
    pub fn kept(&self) -> f64 {
        self.pieces().iter().map(|(a, b)| b - a).sum()
    }

    /// The parts of the sound silenced.
    pub fn silences(&self) -> Vec<(f64, f64)> {
        if !self.sound || self.mute {
            return Vec::new();
        }
        (0..self.parts.len()).filter(|&i| self.parts[i].silent && !self.parts[i].deleted).map(|i| self.bounds(i)).collect()
    }

    /// If a time is in a deleted part, where the part ends.
    pub fn cut_over(&self, t: f64) -> Option<f64> {
        let i = self.part_at(t);
        let (_, to) = self.bounds(i);
        (self.parts[i].deleted && t < to - 0.02).then_some(to)
    }

    /// Whether the sound is silent at a time.
    pub fn silent_at(&self, t: f64) -> bool {
        self.mute || self.parts[self.part_at(t)].silent
    }

    /// How far saving has got, from 0 to 1, if it's saving.
    pub fn progress(&self) -> Option<f32> {
        self.saving.as_ref().map(|p| p.load(Ordering::Relaxed) as f32 / 1000.0)
    }
}

/// What a video holds, as far as trimming goes.
pub struct Probe {
    pub duration: f64,
    pub sound: bool,
}

/// The first video stream and first sound stream of a file, with their types.
struct Streams {
    video: Option<(u32, IMFMediaType)>,
    audio: Option<(u32, IMFMediaType)>,
}

const HNS: f64 = 10_000_000.0;

fn start_up() {
    unsafe {
        let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
    }
}

fn reader(path: &Path) -> Result<IMFSourceReader, String> {
    start_up();
    let url = crate::util::Wide::path(path);
    unsafe { MFCreateSourceReaderFromURL(url.pcwstr(), None) }.map_err(|e| format!("Windows couldn't read the video: {}", e.message()))
}

fn streams(reader: &IMFSourceReader) -> Streams {
    let mut found = Streams { video: None, audio: None };
    for i in 0..32u32 {
        let Ok(t) = (unsafe { reader.GetNativeMediaType(i, 0) }) else { break };
        let Ok(major) = (unsafe { t.GetGUID(&MF_MT_MAJOR_TYPE) }) else { continue };
        if major == MFMediaType_Video && found.video.is_none() {
            found.video = Some((i, t));
        } else if major == MFMediaType_Audio && found.audio.is_none() {
            found.audio = Some((i, t));
        }
    }
    found
}

fn subtype(t: &IMFMediaType) -> GUID {
    unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default()
}

/// Whether a video can be trimmed, and what it holds. Only H.264 and HEVC video go into an MP4
/// as they are; sound that isn't AAC is left out.
pub fn probe(path: &Path) -> Result<Probe, String> {
    let r = reader(path)?;
    let s = streams(&r);
    let Some((_, video)) = &s.video else { return Err("It has no picture to trim.".into()) };
    let v = subtype(video);
    if v != MFVideoFormat_H264 && v != MFVideoFormat_HEVC {
        return Err("View can trim videos made with H.264 or HEVC, which is most phone and camera videos. This one uses something else.".into());
    }
    let duration = unsafe { r.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION) }.ok().and_then(|v| u64::try_from(&v).ok()).map_or(0.0, |d| d as f64 / HNS);
    if duration < SHORTEST * 2.0 {
        return Err("It's too short to trim.".into());
    }
    let sound = s.audio.as_ref().is_some_and(|(_, a)| subtype(a) == MFAudioFormat_AAC);
    Ok(Probe { duration, sound })
}

fn seek(reader: &IMFSourceReader, t: f64) -> windows::core::Result<()> {
    let at = PROPVARIANT::from((t.max(0.0) * HNS) as i64);
    unsafe { reader.SetCurrentPosition(&GUID::zeroed(), &at) }
}

fn is_key(sample: &IMFSample) -> bool {
    // Readers that don't mark key frames are taken to start on one.
    unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.map_or(true, |v| v != 0)
}

/// The times of the first key frames at or after a point the reader is sought to: the one it
/// lands on (at or before), then up to `more` after it.
fn key_frames(path: &Path, t: f64, more: usize) -> Option<Vec<f64>> {
    let r = reader(path).ok()?;
    let (index, _) = streams(&r).video?;
    unsafe {
        r.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false).ok()?;
        r.SetStreamSelection(index, true).ok()?;
    }
    seek(&r, t).ok()?;
    let mut keys = Vec::new();
    for _ in 0..2000 {
        let (mut flags, mut sample) = (0u32, None);
        unsafe { r.ReadSample(index, 0, None, Some(&mut flags), None, Some(&mut sample)) }.ok()?;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            break;
        }
        let Some(sample) = sample else { continue };
        if is_key(&sample) {
            keys.push(unsafe { sample.GetSampleTime() }.ok()? as f64 / HNS);
            if keys.len() > more {
                break;
            }
        }
    }
    (!keys.is_empty()).then_some(keys)
}

/// Where a piece of video starting at `t` really starts: the key frame at or before it.
pub fn key_frame_before(path: &Path, t: f64) -> Option<f64> {
    if t <= 0.0 {
        return Some(0.0);
    }
    let keys = key_frames(path, t, 0)?;
    Some(keys[0].min(t).max(0.0))
}

/// Where a deleted part from `from` to `to` should end, so the video after it starts on a key
/// frame: the key frame before, so a little less goes; or the one after, if that would leave
/// nothing deleted.
pub fn cut_end(path: &Path, from: f64, to: f64) -> Option<f64> {
    let keys = key_frames(path, to, 1)?;
    if keys[0] > from + 0.05 {
        return Some(keys[0].min(to));
    }
    keys.iter().copied().find(|&k| k >= to - 0.001)
}

/// Write the pieces of `from` to `to` as an MP4: picture and sound, with the sound silenced in
/// `silences`, or none if `mute`. `progress` counts up to 1000; `stop` gives up.
pub fn save(from: &Path, to: &Path, pieces: &[(f64, f64)], silences: &[(f64, f64)], mute: bool, progress: &AtomicU32, stop: &AtomicBool) -> Result<(), String> {
    let result = write(from, to, pieces, silences, mute, progress, stop);
    if result.is_err() {
        let _ = std::fs::remove_file(to);
    }
    result
}

fn fail(e: windows::core::Error) -> String {
    format!("Windows couldn't write the video: {}", e.message())
}

fn audio_type(subtype: &GUID, rate: u32, channels: u32, bytes_per_second: u32) -> windows::core::Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bytes_per_second)?;
        if *subtype == MFAudioFormat_PCM {
            t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, channels * 2)?;
            t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        }
        Ok(t)
    }
}

/// Decoded sound: zero what falls in the silences. `t` is when the sample starts.
fn silence(sample: &IMFSample, t: i64, rate: u32, block: u32, silences: &[(i64, i64)]) -> windows::core::Result<()> {
    if silences.is_empty() {
        return Ok(());
    }
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let (mut data, mut len) = (std::ptr::null_mut(), 0u32);
        buffer.Lock(&mut data, None, Some(&mut len))?;
        let frames = (len / block) as i64;
        let end = t + frames * HNS as i64 / rate as i64;
        for &(a, b) in silences {
            if b <= t || a >= end {
                continue;
            }
            let first = ((a - t).max(0) * rate as i64 / HNS as i64).clamp(0, frames);
            let last = ((b - t) * rate as i64 / HNS as i64 + 1).clamp(0, frames);
            if last > first {
                std::ptr::write_bytes(data.add((first * block as i64) as usize), 0, ((last - first) * block as i64) as usize);
            }
        }
        buffer.Unlock()?;
    }
    Ok(())
}

fn write(from: &Path, to: &Path, pieces: &[(f64, f64)], silences: &[(f64, f64)], mute: bool, progress: &AtomicU32, stop: &AtomicBool) -> Result<(), String> {
    if pieces.is_empty() {
        return Err("There's nothing left to save.".into());
    }
    let r = reader(from)?;
    let found = streams(&r);
    let (video, vtype) = found.video.ok_or("It has no picture to trim.")?;
    let audio = found.audio.filter(|(_, t)| !mute && subtype(t) == MFAudioFormat_AAC);
    let select = || -> Result<(), String> {
        unsafe {
            r.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false).map_err(fail)?;
            r.SetStreamSelection(video, true).map_err(fail)?;
            if let Some((a, _)) = &audio {
                r.SetStreamSelection(*a, true).map_err(fail)?;
            }
        }
        Ok(())
    };

    // An MP4 whatever the name.
    let mut attrs = None;
    unsafe { MFCreateAttributes(&mut attrs, 2) }.map_err(fail)?;
    let attrs = attrs.ok_or("Windows couldn't write the video.")?;
    let writer = unsafe {
        attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4).map_err(fail)?;
        attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1).map_err(fail)?;
        let url = crate::util::Wide::path(to);
        MFCreateSinkWriterFromURL(url.pcwstr(), None, &attrs).map_err(fail)?
    };
    // The picture goes in as it is.
    let vtype = unsafe { r.GetCurrentMediaType(video) }.unwrap_or(vtype);
    let out_video = unsafe { writer.AddStream(&vtype) }.map_err(fail)?;
    unsafe { writer.SetInputMediaType(out_video, &vtype, None) }.map_err(fail)?;
    // The sound goes in as it is too, unless some of it is silenced: then it's decoded here and
    // encoded again by the writer.
    let silences: Vec<(i64, i64)> = silences.iter().map(|&(a, b)| ((a * HNS) as i64, (b * HNS) as i64)).collect();
    let mut pcm: Option<(u32, u32)> = None;
    let out_audio = match &audio {
        Some((a, native)) if !silences.is_empty() => {
            let get = |key: &GUID, or: u32| unsafe { native.GetUINT32(key) }.unwrap_or(or);
            let rate = match get(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000) {
                r @ (44100 | 48000) => r,
                _ => 48000,
            };
            let channels = match get(&MF_MT_AUDIO_NUM_CHANNELS, 2) {
                c @ (1 | 2 | 6) => c,
                _ => 2,
            };
            let raw = audio_type(&MFAudioFormat_PCM, rate, channels, rate * channels * 2).map_err(fail)?;
            unsafe { r.SetCurrentMediaType(*a, None, &raw) }.map_err(fail)?;
            let aac = audio_type(&MFAudioFormat_AAC, rate, channels, if channels == 1 { 16000 } else { 24000 }).map_err(fail)?;
            let i = unsafe { writer.AddStream(&aac) }.map_err(fail)?;
            unsafe { writer.SetInputMediaType(i, &raw, None) }.map_err(fail)?;
            pcm = Some((rate, channels * 2));
            Some(i)
        }
        Some((a, native)) => {
            let t = unsafe { r.GetCurrentMediaType(*a) }.unwrap_or_else(|_| native.clone());
            let i = unsafe { writer.AddStream(&t) }.map_err(fail)?;
            unsafe { writer.SetInputMediaType(i, &t, None) }.map_err(fail)?;
            Some(i)
        }
        None => None,
    };
    unsafe { writer.BeginWriting() }.map_err(fail)?;

    let total: f64 = pieces.iter().map(|(a, b)| b - a).sum::<f64>().max(0.001);
    let mut done_before = 0.0;
    // Where the next piece goes in the new file.
    let mut cursor: i64 = 0;
    let mut wrote_any = false;
    let shift = |s: &IMFSample, by: i64| -> windows::core::Result<()> {
        unsafe {
            let t = s.GetSampleTime()?;
            s.SetSampleTime(t + by)?;
            if let Ok(d) = s.GetUINT64(&MFSampleExtension_DecodeTimestamp) {
                s.SetUINT64(&MFSampleExtension_DecodeTimestamp, (d as i64 + by).max(0) as u64)?;
            }
        }
        Ok(())
    };
    for &(a, b) in pieces {
        select()?;
        seek(&r, a).map_err(fail)?;
        let (a_hns, b_hns) = ((a * HNS) as i64, (b * HNS) as i64);
        // Each piece moves so its first key frame follows on from the last piece.
        let mut base: Option<i64> = None;
        // Sound read before the first key frame, held until it's known where that is.
        let mut early: Vec<IMFSample> = Vec::new();
        let mut piece_end = 0i64;
        let (mut video_done, mut audio_done) = (false, out_audio.is_none());
        while !(video_done && audio_done) {
            if stop.load(Ordering::Relaxed) {
                return Err("Stopped.".into());
            }
            let (mut index, mut flags, mut sample) = (0u32, 0u32, None);
            unsafe { r.ReadSample(MF_SOURCE_READER_ANY_STREAM.0 as u32, 0, Some(&mut index), Some(&mut flags), None, Some(&mut sample)) }.map_err(fail)?;
            let is_video = index == video;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                if is_video {
                    video_done = true;
                } else {
                    audio_done = true;
                }
                continue;
            }
            let Some(sample) = sample else { continue };
            let t = unsafe { sample.GetSampleTime() }.map_err(fail)?;
            if is_video {
                if video_done {
                    continue;
                }
                let decode = unsafe { sample.GetUINT64(&MFSampleExtension_DecodeTimestamp) }.map_or(t, |d| d as i64);
                if decode >= b_hns {
                    video_done = true;
                    let _ = unsafe { r.SetStreamSelection(video, false) };
                    continue;
                }
                let b0 = match base {
                    Some(b0) => b0,
                    // Nothing can be shown before the first key frame.
                    None if !is_key(&sample) => continue,
                    None => {
                        let b0 = t.min(decode);
                        base = Some(b0);
                        if let Some(oa) = out_audio {
                            for s in early.drain(..) {
                                let at = unsafe { s.GetSampleTime() }.map_err(fail)?;
                                if at >= b0 {
                                    if let Some((rate, block)) = pcm {
                                        silence(&s, at, rate, block, &silences).map_err(fail)?;
                                    }
                                    shift(&s, cursor - b0).map_err(fail)?;
                                    unsafe { writer.WriteSample(oa, &s) }.map_err(fail)?;
                                }
                            }
                        }
                        b0
                    }
                };
                // Frames shown before the key frame lean on ones before it, which aren't there.
                if t < b0 {
                    continue;
                }
                let length = unsafe { sample.GetSampleDuration() }.unwrap_or(0);
                piece_end = piece_end.max(t + length - b0);
                shift(&sample, cursor - b0).map_err(fail)?;
                unsafe { writer.WriteSample(out_video, &sample) }.map_err(fail)?;
                wrote_any = true;
                let here = ((t - a_hns) as f64 / HNS).clamp(0.0, b - a);
                progress.store(((done_before + here) / total * 990.0).min(990.0) as u32, Ordering::Relaxed);
            } else if let Some(oa) = out_audio {
                if audio_done {
                    continue;
                }
                if t >= b_hns {
                    audio_done = true;
                    let _ = unsafe { r.SetStreamSelection(index, false) };
                    continue;
                }
                match base {
                    None => early.push(sample),
                    Some(b0) if t >= b0 => {
                        if let Some((rate, block)) = pcm {
                            silence(&sample, t, rate, block, &silences).map_err(fail)?;
                        }
                        shift(&sample, cursor - b0).map_err(fail)?;
                        unsafe { writer.WriteSample(oa, &sample) }.map_err(fail)?;
                    }
                    Some(_) => {}
                }
            }
        }
        cursor += piece_end;
        done_before += b - a;
    }
    if !wrote_any {
        return Err("There's no picture in what's kept.".into());
    }
    unsafe { writer.Finalize() }.map_err(fail)?;
    progress.store(1000, Ordering::Relaxed);
    Ok(())
}

/// A time to a tenth of a second: "0:03.2", "1:02:03.5".
pub fn clock(seconds: f64) -> String {
    let tenths = (seconds.max(0.0) * 10.0).round() as u64;
    let (s, t) = (tenths / 10, tenths % 10);
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}.{t}") } else { format!("{m}:{s:02}.{t}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trim(duration: f64) -> Trim {
        Trim::new(Path::new("a.mp4"), Probe { duration, sound: true })
    }

    #[test]
    fn clock_reads_to_a_tenth() {
        assert_eq!(clock(0.0), "0:00.0");
        assert_eq!(clock(3.24), "0:03.2");
        assert_eq!(clock(62.96), "1:03.0");
        assert_eq!(clock(3723.5), "1:02:03.5");
    }

    #[test]
    fn the_ends_stay_apart() {
        let mut t = trim(10.0);
        assert!(!t.has_changes());
        t.set_start(9.9);
        assert_eq!(t.start, 10.0 - SHORTEST);
        t.set_end(1.0);
        assert_eq!(t.end, 10.0);
        t.set_start(2.0);
        t.set_end(2.1);
        assert_eq!(t.end, 2.0 + SHORTEST);
        assert!(t.has_changes());
        t.reset();
        t.mute = true;
        assert!(t.has_changes());
        t.sound = false;
        assert!(!t.has_changes());
    }

    #[test]
    fn splits_make_parts() {
        let mut t = trim(20.0);
        assert_eq!(t.split(5.0), Some(0));
        assert_eq!(t.split(10.0), Some(1));
        assert_eq!(t.split(2.0), Some(0));
        assert_eq!(t.splits, vec![2.0, 5.0, 10.0]);
        assert_eq!(t.parts.len(), 4);
        // Too close to another, or to an end.
        assert_eq!(t.split(5.05), None);
        assert_eq!(t.split(19.95), None);
        assert_eq!(t.part_at(1.0), 0);
        assert_eq!(t.part_at(7.0), 2);
        assert_eq!(t.bounds(2), (5.0, 10.0));
        assert_eq!(t.bounds(3), (10.0, 20.0));
        assert!(!t.has_changes());
        // Delete the part between two splits.
        t.selected = Some(2);
        assert_eq!(t.delete_selected(), Some(2));
        assert_eq!(t.selected, None);
        assert_eq!(t.pieces(), vec![(0.0, 5.0), (10.0, 20.0)]);
        assert!((t.kept() - 15.0).abs() < 1e-9);
        assert_eq!(t.cut_over(7.0), Some(10.0));
        assert_eq!(t.cut_over(12.0), None);
        // Silence another.
        t.selected = Some(0);
        assert_eq!(t.toggle_silent(), Some(true));
        assert_eq!(t.silences(), vec![(0.0, 2.0)]);
        assert!(t.silent_at(1.0));
        assert!(!t.silent_at(3.0));
        assert!(t.has_changes());
        // Splitting a deleted part leaves both halves deleted.
        t.split(8.0);
        assert_eq!(t.pieces(), vec![(0.0, 5.0), (10.0, 20.0)]);
        // The start and end still trim.
        t.start = 1.0;
        t.end = 15.0;
        assert_eq!(t.pieces(), vec![(1.0, 5.0), (10.0, 15.0)]);
        // A split moves only between its neighbours.
        t.set_split(0, 9.0);
        assert_eq!(t.splits[0], 5.0 - CLOSEST);
        t.reset();
        assert_eq!(t.parts.len(), 1);
        assert!(!t.has_changes());
    }

    #[test]
    fn the_bar_closes_up_over_deleted_parts() {
        let mut t = trim(20.0);
        t.split(5.0);
        t.split(10.0);
        assert!((t.length() - 20.0).abs() < 1e-9);
        t.selected = Some(1);
        t.delete_selected();
        assert_eq!(t.shown(), vec![(0.0, 5.0), (10.0, 20.0)]);
        assert!((t.length() - 15.0).abs() < 1e-9);
        assert_eq!(t.to_line(3.0), 3.0);
        assert_eq!(t.to_line(7.0), 5.0);
        assert_eq!(t.to_line(12.0), 7.0);
        assert_eq!(t.from_line(5.0), 10.0);
        assert_eq!(t.from_line(4.0), 4.0);
        assert_eq!(t.from_line(7.0), 12.0);
        assert_eq!(t.from_line(99.0), 20.0);
        assert!(!t.split_shown(0));
        assert!(!t.split_shown(1));
        // The last part left can't go.
        t.selected = Some(0);
        t.delete_selected();
        t.selected = Some(2);
        assert_eq!(t.delete_selected(), None);
    }

    #[test]
    fn undo_and_redo() {
        let mut t = trim(20.0);
        assert!(!t.can_undo());
        t.remember();
        t.split(5.0);
        t.remember();
        t.selected = Some(0);
        t.delete_selected();
        // A note with no change after it is passed over.
        t.remember();
        assert!(t.can_undo());
        assert!(t.undo());
        assert_eq!(t.splits, vec![5.0]);
        assert!(!t.parts[0].deleted);
        assert!(t.undo());
        assert!(t.splits.is_empty());
        assert!(!t.undo());
        assert!(t.redo());
        assert_eq!(t.splits, vec![5.0]);
        assert!(t.redo());
        assert!(t.parts[0].deleted);
        assert!(!t.redo());
        // Something new forgets what was undone.
        t.undo();
        t.remember();
        t.split(15.0);
        assert!(!t.can_redo());
    }

    #[test]
    fn picking_follows_a_split() {
        let mut t = trim(20.0);
        t.split(10.0);
        t.selected = Some(1);
        t.split(5.0);
        assert_eq!(t.selected, Some(2));
        t.split(15.0);
        assert_eq!(t.selected, Some(2));
    }
}
