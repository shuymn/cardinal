//! `listen <path> <seconds>`: accepts connections on a Unix socket at `<path>`. `scripts/test
//! --self-check` starts it outside the tests as the host socket a confined test must not
//! reach. Nothing signals it: it exits once the socket at `<path>` is removed or replaced, once
//! its parent has exited, or after `<seconds>`.

#[cfg(unix)]
fn main() {
    use std::io::ErrorKind;
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, Instant};

    let mut args = std::env::args().skip(1);
    let (Some(path), Some(seconds)) = (args.next(), args.next()) else {
        eprintln!("usage: listen <path> <seconds>");
        std::process::exit(2);
    };
    let seconds: u64 = seconds.parse().expect("a number of seconds");
    let parent = std::os::unix::process::parent_id();
    let listener = std::os::unix::net::UnixListener::bind(&path).expect("listen on the socket");
    listener.set_nonblocking(true).expect("set non-blocking");
    let identity = |meta: &std::fs::Metadata| (meta.dev(), meta.ino());
    let socket = identity(&std::fs::symlink_metadata(&path).expect("stat the socket"));
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline && std::os::unix::process::parent_id() == parent {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if identity(&meta) == socket => {}
            // Removed or replaced: the path is no longer this listener's.
            _ => return,
        }
        match listener.accept() {
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("accept: {err}"),
        }
    }
    if std::fs::symlink_metadata(&path).is_ok_and(|meta| identity(&meta) == socket) {
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("listen: Unix sockets only");
    std::process::exit(2);
}
