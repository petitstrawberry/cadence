//! Progressive HTTP media with a fixed-size read window and validated byte ranges.
//! No media bytes are written to disk or accumulated in an unbounded queue.

use crate::{Result, check_cancel, net::http_url};
use cadence_core::MediaSource;
use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

const RANGE_BYTES: u64 = 256 * 1024;
const READ_BYTES: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OPEN_WORKERS: usize = 16;
static OPEN_WORKERS: AtomicUsize = AtomicUsize::new(0);

// ureq's socket timeout is an inactivity timeout. Opening on a bounded worker
// also bounds the caller's wait when DNS or trickled headers never finish. A
// cancelled receiver releases any response as soon as that worker returns.
// The permit stays with the worker, preventing repeated stalled peers from
// creating an unbounded number of detached threads.
struct OpenPermit;
impl OpenPermit {
    fn acquire() -> Result<Self> {
        OPEN_WORKERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_OPEN_WORKERS).then_some(count + 1)
            })
            .map(|_| Self)
            .map_err(|_| "Too many source connections are still opening".into())
    }
}
impl Drop for OpenPermit {
    fn drop(&mut self) {
        OPEN_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn open_response(
    request: ureq::Request,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<ureq::Response> {
    bounded_open(
        cancel,
        deadline,
        "Source response headers timed out",
        move || match request.call() {
            Ok(reply) => Ok(reply),
            Err(ureq::Error::Status(code, _)) => Err(format!("Source returned HTTP {code}")),
            Err(ureq::Error::Transport(_)) => Err("Source connection failed or timed out".into()),
        },
    )
}

/// Cover provider discovery, DNS and credential resolution too. Built-ins still
/// observe cancellation themselves; this keeps third-party/provider setup from
/// pinning the playback worker if a blocking operating-system call stalls.
pub(crate) fn open_source(
    cancel: Arc<AtomicBool>,
    open: impl FnOnce(Arc<AtomicBool>) -> Result<Box<dyn MediaSource>> + Send + 'static,
) -> Result<Box<dyn MediaSource>> {
    let worker_cancel = cancel.clone();
    let result = bounded_open(
        &cancel,
        Instant::now() + Duration::from_secs(20),
        "Source media resolution timed out",
        move || open(worker_cancel),
    );
    if result.is_err() {
        // DNS may finish after the caller's deadline. Prevent that late worker
        // from creating or retaining a session after this open has failed.
        cancel.store(true, Ordering::Release);
    }
    result
}

fn bounded_open<T: Send + 'static>(
    cancel: &AtomicBool,
    deadline: Instant,
    timeout_message: &'static str,
    open: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    check_cancel(cancel)?;
    let permit = OpenPermit::acquire()?;
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("cadence-source-open".into())
        .spawn(move || {
            let _permit = permit;
            let _ = sender.send(open());
        })
        .map_err(|_| "Cannot start source connection worker")?;
    loop {
        check_cancel(cancel)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timeout_message.into());
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(25))) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => (),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("Source connection worker stopped".into());
            }
        }
    }
}

/// Session servicing is independent of media consumption: pause must not let a
/// WebSocket heartbeat or executable-provider lease expire.
pub(crate) struct SessionLease {
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    worker: Option<JoinHandle<()>>,
}

impl SessionLease {
    pub(crate) fn spawn(
        cancel: Arc<AtomicBool>,
        mut pump: impl FnMut() -> Result<()> + Send + 'static,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let worker_stop = stop.clone();
        let worker_failure = failure.clone();
        let worker = std::thread::Builder::new()
            .name("cadence-source-session".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
                    if let Err(error) = pump() {
                        *worker_failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            })
            .map_err(|_| "Cannot start source session worker")?;
        Ok(Self {
            stop,
            failure,
            worker: Some(worker),
        })
    }

    fn check(&self) -> io::Result<()> {
        match self
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(error) => Err(io::Error::other(error.clone())),
            None => Ok(()),
        }
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct ResponseBody {
    reader: Box<dyn Read + Send + Sync>,
    remaining: Option<u64>,
    length: Option<u64>,
    ranged: bool,
    validator: Option<Validator>,
}

#[derive(Clone, PartialEq, Eq)]
enum Validator {
    Etag(String),
    LastModified(String),
}
impl Validator {
    fn from_response(reply: &ureq::Response) -> Option<Self> {
        reply
            .header("ETag")
            .filter(|etag| !etag.starts_with("W/"))
            .map(|etag| Self::Etag(etag.to_owned()))
            .or_else(|| {
                reply
                    .header("Last-Modified")
                    .map(|value| Self::LastModified(value.to_owned()))
            })
    }
    fn value(&self) -> &str {
        match self {
            Self::Etag(value) | Self::LastModified(value) => value,
        }
    }
}

pub(crate) struct HttpStream {
    // Deliberately no Debug implementation: signed URLs belong only to this lease.
    url: String,
    client: ureq::Agent,
    body: ResponseBody,
    position: u64,
    cancel: Arc<AtomicBool>,
    lease: Option<SessionLease>,
}

impl HttpStream {
    pub(crate) fn open(url: &str, cancel: Arc<AtomicBool>) -> Result<Self> {
        check_cancel(&cancel)?;
        http_url(url)?;
        let client = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(READ_TIMEOUT)
            .timeout_write(READ_TIMEOUT)
            .redirects(0)
            .build();
        let body = request(&client, url, 0, None, None, &cancel)?;
        check_cancel(&cancel)?;
        Ok(Self {
            url: url.into(),
            client,
            body,
            position: 0,
            cancel,
            lease: None,
        })
    }

    pub(crate) fn with_lease(mut self, lease: SessionLease) -> Self {
        self.lease = Some(lease);
        self
    }

    fn check(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "Source request cancelled",
            ));
        }
        if let Some(lease) = &self.lease {
            lease.check()?;
        }
        Ok(())
    }

    fn reopen(&mut self, offset: u64) -> io::Result<()> {
        self.check()?;
        let body = request(
            &self.client,
            &self.url,
            offset,
            self.body.length,
            self.body.validator.as_ref(),
            &self.cancel,
        )
        .map_err(io::Error::other)?;
        // A server silently ignoring Range or changed object must never splice
        // bytes from the beginning of another representation into the decoder.
        if !body.ranged {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Source stopped honoring byte ranges",
            ));
        }
        self.check()?;
        self.body = body;
        self.position = offset;
        Ok(())
    }
}

impl Read for HttpStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.body.remaining == Some(0) {
            if self.body.ranged
                && self
                    .body
                    .length
                    .is_some_and(|length| self.position < length)
            {
                self.reopen(self.position)?;
            } else {
                return Ok(0);
            }
        }
        let limit = buffer.len().min(READ_BYTES).min(
            self.body
                .remaining
                .unwrap_or(u64::MAX)
                .min(usize::MAX as u64) as usize,
        );
        let result = self.body.reader.read(&mut buffer[..limit]);
        self.check()?;
        let count = result.map_err(|error| {
            // Several demuxers treat UnexpectedEof as a normal media ending.
            // A transport failure must reach the player as an actual error.
            let kind = match error.kind() {
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => io::ErrorKind::TimedOut,
                _ => io::ErrorKind::InvalidData,
            };
            io::Error::new(kind, "Source stream read failed or timed out")
        })?;
        if count == 0 && self.body.remaining.is_some_and(|remaining| remaining > 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Source returned a truncated media response",
            ));
        }
        self.position = self.position.checked_add(count as u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Source position overflow")
        })?;
        if let Some(remaining) = &mut self.body.remaining {
            *remaining -= count as u64;
        }
        Ok(count)
    }
}

impl Seek for HttpStream {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.check()?;
        let offset = match from {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
            SeekFrom::End(delta) => self
                .body
                .length
                .and_then(|length| length.checked_add_signed(delta)),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid media seek offset"))?;
        if offset == self.position {
            return Ok(offset);
        }
        if !self.body.ranged {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "This source does not support byte-range seeking",
            ));
        }
        let length = self.body.length.expect("validated ranged length");
        if offset > length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Media seek exceeds source length",
            ));
        }
        if offset == length {
            // Replace the reader so an abandoned response releases its socket.
            self.body.reader = Box::new(io::empty());
            self.body.remaining = Some(0);
            self.position = offset;
        } else {
            self.reopen(offset)?;
        }
        Ok(offset)
    }
}

impl MediaSource for HttpStream {
    fn is_seekable(&self) -> bool {
        self.body.ranged
    }
    fn byte_len(&self) -> Option<u64> {
        self.body.length
    }
}

fn request(
    client: &ureq::Agent,
    url: &str,
    start: u64,
    previous_length: Option<u64>,
    validator: Option<&Validator>,
    cancel: &AtomicBool,
) -> Result<ResponseBody> {
    let mut current = http_url(url)?;
    let end = start.saturating_add(RANGE_BYTES - 1);
    let deadline = Instant::now() + HEADER_TIMEOUT;
    for _ in 0..=5 {
        check_cancel(cancel)?;
        let mut request = client
            .get(current.as_str())
            .set("Accept-Encoding", "identity")
            .set("Range", &format!("bytes={start}-{end}"));
        if let Some(validator) = validator {
            request = request.set("If-Range", validator.value());
        }
        let reply = open_response(request, cancel, deadline)?;
        check_cancel(cancel)?;
        if matches!(reply.status(), 301 | 302 | 303 | 307 | 308) {
            let destination = reply
                .header("Location")
                .ok_or("Source redirect has no destination")?;
            let destination = current
                .join(destination)
                .map_err(|_| "Source redirect is invalid")?;
            if current.scheme() == "https" && destination.scheme() != "https" {
                return Err("Source tried to redirect HTTPS media to an insecure endpoint".into());
            }
            current = http_url(destination.as_str())?;
            continue;
        }
        if reply
            .header("Content-Encoding")
            .is_some_and(|value| !value.eq_ignore_ascii_case("identity"))
        {
            return Err("Source returned encoded media despite an identity request".into());
        }
        let declared = reply
            .header("Content-Length")
            .map(|length| {
                length
                    .parse::<u64>()
                    .map_err(|_| "Source returned an invalid media length")
            })
            .transpose()?;
        let actual_validator = Validator::from_response(&reply);
        if validator.is_some_and(|old| Some(old) != actual_validator.as_ref()) {
            return Err("Source media changed while streaming".into());
        }
        let (length, remaining, ranged) = match reply.status() {
            200 if start == 0 && previous_length.is_none() => {
                if reply.header("Content-Range").is_some() {
                    return Err("Source returned an unexpected byte range".into());
                }
                (declared, declared, false)
            }
            206 => {
                let (first, last, total) = parse_range(
                    reply
                        .header("Content-Range")
                        .ok_or("Source omitted Content-Range")?,
                )?;
                if first != start
                    || last > end
                    || previous_length.is_some_and(|length| length != total)
                {
                    return Err("Source returned an inconsistent media byte range".into());
                }
                let count = last - first + 1;
                if declared.is_some_and(|length| length != count) {
                    return Err("Source byte range does not match its Content-Length".into());
                }
                (Some(total), Some(count), true)
            }
            code => {
                return Err(format!(
                    "Source did not honor media byte range (HTTP {code})"
                ));
            }
        };
        return Ok(ResponseBody {
            reader: reply.into_reader(),
            remaining,
            length,
            ranged,
            validator: actual_validator,
        });
    }
    Err("Source redirected too many times".into())
}

fn parse_range(value: &str) -> Result<(u64, u64, u64)> {
    let value = value
        .strip_prefix("bytes ")
        .ok_or("Source returned an invalid Content-Range")?;
    let (range, length) = value
        .split_once('/')
        .ok_or("Source returned an invalid Content-Range")?;
    let (start, end) = range
        .split_once('-')
        .ok_or("Source returned an invalid Content-Range")?;
    let invalid = || "Source returned an invalid Content-Range";
    let start: u64 = start.parse().map_err(|_| invalid())?;
    let end: u64 = end.parse().map_err(|_| invalid())?;
    let length: u64 = length.parse().map_err(|_| invalid())?;
    if start > end || end >= length {
        return Err(invalid().into());
    }
    Ok((start, end, length))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        net::{TcpListener, TcpStream},
        sync::mpsc,
        time::Instant,
    };

    fn accept(listener: &TcpListener) -> TcpStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match listener.accept() {
                Ok((socket, _)) => {
                    socket.set_nonblocking(false).unwrap();
                    return socket;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "stream did not connect");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("fixture accept failed: {error}"),
            }
        }
    }

    fn receive_request(socket: &mut TcpStream) -> String {
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        loop {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
            if bytes.ends_with(b"\r\n\r\n") {
                break;
            }
            assert!(bytes.len() < 16 * 1024);
        }
        String::from_utf8(bytes).unwrap()
    }

    fn server(response: &'static str) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/media?signature=do-not-expose",
            listener.local_addr().unwrap()
        );
        let worker = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            receive_request(&mut socket);
            socket.write_all(response.as_bytes()).unwrap();
        });
        (url, worker)
    }

    fn remote_track(url: String, extension: &str) -> cadence_core::Track {
        let mut track = cadence_core::Track::from_path(std::path::PathBuf::new());
        track.media_input = Some(cadence_core::MediaInput::new(
            "http-fixture",
            Some(extension.to_owned()),
            move |cancel| Ok(Box::new(HttpStream::open(&url, cancel)?)),
        ));
        track
    }

    #[test]
    fn decoder_produces_audio_before_http_body_completes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (release, wait) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            receive_request(&mut socket);
            let data_bytes = 44_100u32 * 4 * 60;
            let mut header = b"RIFF".to_vec();
            header.extend_from_slice(&(36 + data_bytes).to_le_bytes());
            header.extend_from_slice(b"WAVEfmt ");
            header.extend_from_slice(&16u32.to_le_bytes());
            header.extend_from_slice(&1u16.to_le_bytes());
            header.extend_from_slice(&2u16.to_le_bytes());
            header.extend_from_slice(&44_100u32.to_le_bytes());
            header.extend_from_slice(&176_400u32.to_le_bytes());
            header.extend_from_slice(&4u16.to_le_bytes());
            header.extend_from_slice(&16u16.to_le_bytes());
            header.extend_from_slice(b"data");
            header.extend_from_slice(&data_bytes.to_le_bytes());
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                data_bytes + 44
            );
            socket.write_all(reply.as_bytes()).unwrap();
            socket.write_all(&header).unwrap();
            // Only the first fraction of this 60-second track exists on the
            // wire until the caller has actually decoded its first frames.
            socket.write_all(&vec![0x10; 128 * 1024]).unwrap();
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let track = remote_track(url, "wav");
        let mut decoder = cadence_core::DecoderRegistry::default()
            .open_track(&track, Arc::new(AtomicBool::new(false)))
            .unwrap();
        let frames = decoder.read_frames(512).unwrap();
        assert_eq!(frames.len(), 512);
        assert!(frames.iter().any(|frame| frame[0].abs() > 0.01));
        release.send(()).unwrap();
        drop(decoder);
        worker.join().unwrap();
    }

    #[test]
    fn truncated_http_mp3_is_a_decoder_error_not_a_clean_track_end() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let bytes = include_bytes!("../../cadence-core/tests/fixtures/tone-mono.mp3");
            let mut socket = accept(&listener);
            receive_request(&mut socket);
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            socket.write_all(reply.as_bytes()).unwrap();
            socket.write_all(&bytes[..bytes.len() - 1000]).unwrap();
        });
        let track = remote_track(url, "mp3");
        let mut decoder = cadence_core::DecoderRegistry::default()
            .open_track(&track, Arc::new(AtomicBool::new(false)))
            .expect("valid MP3 prefix must open before the truncated tail");
        assert!(!decoder.read_frames(512).unwrap().is_empty());
        loop {
            match decoder.read_frames(512) {
                Ok(frames) => assert!(!frames.is_empty(), "truncated HTTP MP3 was accepted as EOF"),
                Err(error) => {
                    assert!(
                        error.contains("Source"),
                        "expected source truncation: {error}"
                    );
                    break;
                }
            }
        }
        worker.join().unwrap();
    }

    #[test]
    fn reads_initial_bytes_before_delayed_tail_without_a_cache_file() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (release, wait) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            let request = receive_request(&mut socket);
            assert!(request.contains("Range: bytes=0-262143"));
            assert!(request.contains("Accept-Encoding: identity"));
            socket.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 8\r\nContent-Range: bytes 0-7/8\r\nConnection: close\r\n\r\nhead").unwrap();
            wait.recv_timeout(Duration::from_secs(3)).unwrap();
            socket.write_all(b"tail").unwrap();
        });
        let mut stream = HttpStream::open(&url, Arc::new(AtomicBool::new(false))).unwrap();
        let mut head = [0; 4];
        stream.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"head");
        release.send(()).unwrap();
        let mut tail = Vec::new();
        stream.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, b"tail");
        worker.join().unwrap();
    }

    #[test]
    fn ranges_support_forward_backward_end_and_window_boundaries() {
        let media: Arc<Vec<u8>> = Arc::new(
            (0..(RANGE_BYTES + 23))
                .map(|value| (value % 251) as u8)
                .collect(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server_media = media.clone();
        let worker = std::thread::spawn(move || {
            // Initial + next sequential window + backward + forward + end-relative.
            for index in 0..5 {
                let mut socket = accept(&listener);
                let request = receive_request(&mut socket);
                let range = request
                    .lines()
                    .find_map(|line| line.strip_prefix("Range: bytes="))
                    .unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let start: usize = start.parse().unwrap();
                let end: usize = end.parse::<usize>().unwrap().min(server_media.len() - 1);
                if index != 0 {
                    assert!(request.contains("If-Range: \"version-one\""));
                }
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nETag: \"version-one\"\r\nConnection: close\r\n\r\n",
                    end - start + 1,
                    server_media.len()
                );
                socket.write_all(header.as_bytes()).unwrap();
                // A seek can abandon a previous response, which is expected.
                let _ = socket.write_all(&server_media[start..=end]);
            }
        });
        let mut stream = HttpStream::open(&url, Arc::new(AtomicBool::new(false))).unwrap();
        assert!(stream.is_seekable());
        assert_eq!(stream.byte_len(), Some(media.len() as u64));
        let mut all = Vec::new();
        stream.read_to_end(&mut all).unwrap();
        assert_eq!(all, *media);
        assert_eq!(stream.seek(SeekFrom::Start(7)).unwrap(), 7);
        let mut bytes = [0; 3];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, media[7..10]);
        assert_eq!(stream.seek(SeekFrom::Current(17)).unwrap(), 27);
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, media[27..30]);
        stream.seek(SeekFrom::End(-3)).unwrap();
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, media[media.len() - 3..]);
        assert_eq!(stream.seek(SeekFrom::End(0)).unwrap(), media.len() as u64);
        assert_eq!(stream.read(&mut bytes).unwrap(), 0);
        assert!(stream.seek(SeekFrom::End(1)).is_err());
        worker.join().unwrap();
    }

    #[test]
    fn ignored_ranges_are_truthfully_forward_only() {
        let (url, worker) = server(
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\nwave",
        );
        let mut stream = HttpStream::open(&url, Arc::new(AtomicBool::new(false))).unwrap();
        assert!(!stream.is_seekable());
        assert_eq!(stream.byte_len(), Some(4));
        let mut bytes = [0; 2];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(stream.stream_position().unwrap(), 2);
        assert_eq!(
            stream.seek(SeekFrom::Start(0)).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ve");
        worker.join().unwrap();
    }

    #[test]
    fn invalid_partial_responses_are_rejected_without_exposing_signed_url() {
        for response in [
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nConnection: close\r\n\r\naa",
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nContent-Range: bytes 1-2/3\r\nConnection: close\r\n\r\naa",
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nContent-Range: bytes 0-2/2\r\nConnection: close\r\n\r\naa",
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nContent-Range: bytes 0-2/3\r\nConnection: close\r\n\r\naa",
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nContent-Range: bytes 0-1/*\r\nConnection: close\r\n\r\naa",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Encoding: gzip\r\nConnection: close\r\n\r\naa",
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ] {
            let (url, worker) = server(response);
            let error = HttpStream::open(&url, Arc::new(AtomicBool::new(false)))
                .err()
                .unwrap();
            assert!(!error.contains("signature"));
            assert!(!error.contains("127.0.0.1"));
            worker.join().unwrap();
        }
    }

    #[test]
    fn changed_identity_or_size_and_ignored_subsequent_range_are_rejected() {
        for changed in [
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 4-7/8\r\nETag: \"two\"\r\nConnection: close\r\n\r\ndefg",
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 4-7/9\r\nETag: \"one\"\r\nConnection: close\r\n\r\ndefg",
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 4-7/8\r\nConnection: close\r\n\r\ndefg",
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nETag: \"one\"\r\nConnection: close\r\n\r\nabcdefgh",
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let worker = std::thread::spawn(move || {
                for reply in [
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: 8\r\nContent-Range: bytes 0-7/8\r\nETag: \"one\"\r\nConnection: close\r\n\r\nabcdefgh",
                    changed,
                ] {
                    let mut socket = accept(&listener);
                    receive_request(&mut socket);
                    socket.write_all(reply.as_bytes()).unwrap();
                }
            });
            let mut stream = HttpStream::open(&url, Arc::new(AtomicBool::new(false))).unwrap();
            assert!(stream.seek(SeekFrom::Start(4)).is_err());
            // A failed seek leaves the original reader and offset usable.
            assert_eq!(stream.stream_position().unwrap(), 0);
            worker.join().unwrap();
        }
    }

    #[test]
    fn premature_eof_is_an_error_for_full_and_partial_bodies() {
        for response in [
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nshort",
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 8\r\nContent-Range: bytes 0-7/8\r\nConnection: close\r\n\r\nshort",
        ] {
            let (url, worker) = server(response);
            let mut stream = HttpStream::open(&url, Arc::new(AtomicBool::new(false))).unwrap();
            assert_eq!(
                stream.read_to_end(&mut Vec::new()).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            worker.join().unwrap();
        }
    }

    #[test]
    fn provider_resolution_cancellation_releases_playback_before_stalled_setup_finishes() {
        struct DropBytes(mpsc::SyncSender<()>);
        impl AsRef<[u8]> for DropBytes {
            fn as_ref(&self) -> &[u8] {
                b"fixture"
            }
        }
        impl Drop for DropBytes {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (finished_sender, finished_receiver) = mpsc::sync_channel(1);
        let signal = cancel.clone();
        let cancelling = std::thread::spawn(move || {
            started_receiver
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
            signal.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let result = open_source(cancel, move |_| {
            started_sender.send(()).unwrap();
            release_receiver
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
            Ok(Box::new(io::Cursor::new(DropBytes(finished_sender))))
        });
        assert!(matches!(result, Err(error) if error.contains("cancelled")));
        assert!(started.elapsed() < Duration::from_secs(1));
        release_sender.send(()).unwrap();
        cancelling.join().unwrap();
        finished_receiver
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
    }

    #[test]
    fn trickled_headers_do_not_block_cancellation_or_header_deadline() {
        for should_cancel in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let stop = Arc::new(AtomicBool::new(false));
            let server_stop = stop.clone();
            let (started_sender, started_receiver) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let mut socket = accept(&listener);
                receive_request(&mut socket);
                socket.write_all(b"HTTP/1.1 200 OK\r\nX-Slow: ").unwrap();
                started_sender.send(()).unwrap();
                while !server_stop.load(Ordering::Relaxed) {
                    if socket.write_all(b"x").is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
            let cancel = Arc::new(AtomicBool::new(false));
            let cancel_signal = cancel.clone();
            let cancelling = std::thread::spawn(move || {
                started_receiver
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
                if should_cancel {
                    std::thread::sleep(Duration::from_millis(50));
                    cancel_signal.store(true, Ordering::Relaxed);
                }
            });
            let started = Instant::now();
            let deadline = started
                + if should_cancel {
                    Duration::from_secs(3)
                } else {
                    Duration::from_millis(150)
                };
            let request = ureq::AgentBuilder::new()
                .timeout_read(READ_TIMEOUT)
                .build()
                .get(&url);
            let error = open_response(request, &cancel, deadline).err().unwrap();
            assert!(started.elapsed() < Duration::from_secs(1));
            assert!(error.contains(if should_cancel {
                "cancelled"
            } else {
                "headers timed out"
            }));
            stop.store(true, Ordering::Relaxed);
            cancelling.join().unwrap();
            worker.join().unwrap();
        }
    }

    #[test]
    fn cancellation_is_not_retried_by_read_exact_and_stalled_reads_are_bounded() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (release, wait) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            receive_request(&mut socket);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n")
                .unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(5));
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let mut stream = HttpStream::open(&url, cancel.clone()).unwrap();
        let signal = cancel.clone();
        let cancelling = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(75));
            signal.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let error = stream.read_exact(&mut [0; 4]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert!(started.elapsed() < READ_TIMEOUT + Duration::from_secs(1));
        assert!(stream.seek(SeekFrom::Start(0)).is_err());
        release.send(()).unwrap();
        cancelling.join().unwrap();
        worker.join().unwrap();
        assert!(HttpStream::open("http://127.0.0.1:1/?secret=hidden", cancel).is_err());
    }
}
