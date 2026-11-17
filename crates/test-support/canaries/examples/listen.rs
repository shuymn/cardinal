//! `listen <path> <seconds>`: accepts connections on a Unix socket at `<path>` until it is
//! killed or `<seconds>` have passed, then removes the socket. `scripts/test --self-check`
//! starts it outside the tests as the host socket a confined test must not reach.

#[cfg(unix)]
fn main() {
    use std::io::ErrorKind;
    use std::time::{Duration, Instant};

    let mut args = std::env::args().skip(1);
    let (Some(path), Some(seconds)) = (args.next(), args.next()) else {
        eprintln!("usage: listen <path> <seconds>");
        std::process::exit(2);
    };
    let seconds: u64 = seconds.parse().expect("a number of seconds");
    let listener = std::os::unix::net::UnixListener::bind(&path).expect("listen on the socket");
    listener.set_nonblocking(true).expect("set non-blocking");
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        match listener.accept() {
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("accept: {err}"),
        }
    }
    let _ = std::fs::remove_file(&path);
}

#[cfg(not(unix))]
fn main() {
    eprintln!("listen: Unix sockets only");
    std::process::exit(2);
}
