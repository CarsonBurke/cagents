//! Absolute-budget framing for read-only holder census. Partial progress never
//! renews its budget, and nonblocking descriptors keep socket syscalls finite.
use serde::{Serialize,de::DeserializeOwned};
use std::{io::{Read,Write},os::{fd::AsRawFd,unix::net::UnixStream},time::{Duration,Instant}};

fn remaining(deadline:Instant)->Result<Duration,String> {
    deadline.checked_duration_since(Instant::now()).filter(|left|!left.is_zero())
        .ok_or_else(||"Retained actor inspection deadline expired".into())
}
fn ready(control:&UnixStream,events:libc::c_short,deadline:Instant)->Result<(),String> {
    loop {
        let left=remaining(deadline)?;
        let mut event=libc::pollfd {fd:control.as_raw_fd(),events,revents:0};
        let millis=left.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
        // SAFETY: one initialized pollfd remains live for this finite syscall.
        let result=unsafe {libc::poll(&mut event,1,millis)};
        if result==0 {continue;}
        if result<0 {
            let error=std::io::Error::last_os_error();
            if error.kind()==std::io::ErrorKind::Interrupted {continue;}
            return Err(error.to_string());
        }
        if event.revents & libc::POLLNVAL!=0 {return Err("Retained actor inspection descriptor closed".into());}
        return Ok(());
    }
}
fn write_exact_until(control:&UnixStream,mut bytes:&[u8],deadline:Instant)->Result<(),String> {
    let mut writer=control;
    while !bytes.is_empty() {
        remaining(deadline)?;
        match writer.write(bytes) {
            Ok(0)=>return Err("Retained actor inspection socket stopped accepting data".into()),
            Ok(count)=>bytes=&bytes[count..],
            Err(error) if error.kind()==std::io::ErrorKind::Interrupted=>{},
            Err(error) if error.kind()==std::io::ErrorKind::WouldBlock=>ready(control,libc::POLLOUT,deadline)?,
            Err(error)=>return Err(error.to_string()),
        }
    }
    Ok(())
}
fn read_exact_until(control:&UnixStream,mut bytes:&mut[u8],deadline:Instant)->Result<(),String> {
    let mut reader=control;
    while !bytes.is_empty() {
        remaining(deadline)?;
        match reader.read(bytes) {
            Ok(0)=>return Err("Retained actor inspection response ended early".into()),
            Ok(count)=>bytes=&mut bytes[count..],
            Err(error) if error.kind()==std::io::ErrorKind::Interrupted=>{},
            Err(error) if error.kind()==std::io::ErrorKind::WouldBlock=>ready(control,libc::POLLIN,deadline)?,
            Err(error)=>return Err(error.to_string()),
        }
    }
    Ok(())
}
pub(super) fn send_until(control:&UnixStream,value:&impl Serialize,deadline:Instant)->Result<(),String> {
    remaining(deadline)?;
    let bytes=serde_json::to_vec(value).map_err(|error|error.to_string())?;
    if bytes.len()>super::MAX_FRAME {return Err("Retained actor frame exceeds the bounded protocol capacity".into());}
    control.set_nonblocking(true).map_err(|error|error.to_string())?;
    write_exact_until(control,&(bytes.len() as u32).to_le_bytes(),deadline)?;
    write_exact_until(control,&bytes,deadline)
}
pub(super) fn receive_until<T:DeserializeOwned>(control:&UnixStream,deadline:Instant)->Result<T,String> {
    remaining(deadline)?;
    control.set_nonblocking(true).map_err(|error|error.to_string())?;
    let mut header=[0;4];read_exact_until(control,&mut header,deadline)?;
    let size=u32::from_le_bytes(header) as usize;
    if size>super::MAX_FRAME {return Err("Retained actor frame exceeds the bounded protocol capacity".into());}
    let mut bytes=vec![0;size];read_exact_until(control,&mut bytes,deadline)?;
    remaining(deadline)?;
    let value=serde_json::from_slice(&bytes).map_err(|error|error.to_string())?;
    remaining(deadline)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_frame_progress_does_not_extend_the_absolute_deadline() {
        let (reader,mut writer)=UnixStream::pair().unwrap();
        let (release,wait)=crossbeam_channel::bounded(1);
        let peer=std::thread::spawn(move|| {
            let bytes=serde_json::to_vec(&"x".repeat(128)).unwrap();
            writer.write_all(&(bytes.len() as u32).to_le_bytes()).unwrap();
            for byte in bytes {
                if writer.write_all(&[byte]).is_err() {break;}
                if wait.recv_timeout(Duration::from_millis(5)).is_ok() {break;}
            }
        });
        let started=Instant::now();let error=receive_until::<serde_json::Value>(&reader,started+Duration::from_millis(40)).unwrap_err();
        let elapsed=started.elapsed();let _=release.send(());drop(reader);peer.join().unwrap();
        assert!(error.contains("deadline"));assert!(elapsed<Duration::from_millis(250));
    }
    #[test]
    fn stalled_frame_write_is_bounded_and_oversized_response_is_refused_before_body_read() {
        let (writer,reader)=UnixStream::pair().unwrap();
        let buffer:libc::c_int=4096;
        assert_eq!(unsafe {libc::setsockopt(writer.as_raw_fd(),libc::SOL_SOCKET,libc::SO_SNDBUF,(&raw const buffer).cast(),std::mem::size_of_val(&buffer) as libc::socklen_t)},0);
        let started=Instant::now();assert!(send_until(&writer,&"x".repeat(1024*1024),started+Duration::from_millis(40)).unwrap_err().contains("deadline"));
        assert!(started.elapsed()<Duration::from_millis(250));drop(reader);drop(writer);
        let (reader,mut writer)=UnixStream::pair().unwrap();
        writer.write_all(&((super::super::MAX_FRAME+1) as u32).to_le_bytes()).unwrap();
        assert!(receive_until::<serde_json::Value>(&reader,Instant::now()+Duration::from_secs(1)).unwrap_err().contains("capacity"));
    }
}
