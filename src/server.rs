//! The TCP front door: accepts clients, one thread per connection, with
//! keep-alive. Reads requests, hands them to the [`Proxy`], writes answers.

use std::io::{self, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::http::{read_request, write_response, Response};
use crate::proxy::Proxy;

/// Connections above this number get a quick `503` instead of a thread.
pub const MAX_CONNECTIONS: usize = 1024;
/// Largest request body we accept from a client.
pub const MAX_REQUEST_BODY: usize = 10 * 1024 * 1024;
/// A client that is silent for this long is disconnected.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Accept connections forever.
pub fn serve(proxy: Arc<Proxy>, listener: TcpListener) -> io::Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            reject_busy(stream);
            continue;
        }
        let proxy = proxy.clone();
        let active_for_thread = active.clone();
        let spawned = thread::Builder::new().spawn(move || {
            let _release = Release(active_for_thread);
            handle_connection(&proxy, stream);
        });
        if spawned.is_err() {
            active.fetch_sub(1, Ordering::SeqCst);
            eprintln!("could not start a thread for a new connection");
        }
    }
    Ok(())
}

/// Gives the connection slot back when the thread ends, even on panic.
struct Release(Arc<AtomicUsize>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn reject_busy(mut stream: TcpStream) {
    let resp = Response::text(503, "Service Unavailable", "Too many connections.\n");
    let _ = write_response(&mut stream, &resp, false, false);
}

/// Serve one client connection until it closes, times out or errors.
pub fn handle_connection(proxy: &Proxy, stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(IDLE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IDLE_TIMEOUT));
    let _ = stream.set_nodelay(true);
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);

    loop {
        let req = match read_request(&mut reader, MAX_REQUEST_BODY) {
            Ok(Some(r)) => r,
            Ok(None) => return, // client closed the connection
            Err(e) => {
                // a timeout or a reset just ends the connection quietly;
                // anything else was a bad request
                if e.kind() == io::ErrorKind::InvalidData {
                    let resp = Response::text(400, "Bad Request", "Bad Request\n");
                    let _ = write_response(&mut writer, &resp, false, false);
                }
                return;
            }
        };
        let keep_alive = !req.wants_close();
        let resp = proxy.handle(&req);
        let head_only = req.method == "HEAD";
        if write_response(&mut writer, &resp, head_only, keep_alive).is_err() || !keep_alive {
            let _ = writer.flush();
            return;
        }
    }
}
