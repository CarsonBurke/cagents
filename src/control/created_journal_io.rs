//! Bounded private original-journal response, followed by six original rights.
//! The normal host challenge comes from the sole framed writer FIRST. Neither
//! this codec nor decoded bytes authenticate a peer or authorize a directory.
//! A phase error is fatal: caller MUST abort delivery and drop ALL connection
//! owners, including writer clones, before retrying on a fresh connection.
use std::{io,os::{fd::{AsRawFd,BorrowedFd},unix::net::UnixStream},time::Instant};
use super::{created_journal::{UiJournalChallenge,UiJournalResponse,MAX_CREATION_ROW_BYTES},created_root_rights};
const MAGIC:[u8;4]=*b"CJR1";
const MAX_CHALLENGE_BYTES:usize=4096;

/// UI calls this only with the original producer's checked response after the
/// exact host challenge. It serializes private bytes and rights on ONE stream.
pub(crate) fn send_response(stream:&UnixStream,response:UiJournalResponse,
    files:&[BorrowedFd<'_>;6],deadline:Instant)->io::Result<()> {
    send_response_cancellable(stream,response,files,deadline,&crate::process::CancellationToken::new())
}
pub(crate) fn send_response_cancellable(stream:&UnixStream,response:UiJournalResponse,
    files:&[BorrowedFd<'_>;6],deadline:Instant,cancellation:&crate::process::CancellationToken)->io::Result<()> {
    let result=(|| {
        let (challenge,row)=response.into_wire();
        challenge.validate_shape()?;
        let metadata=postcard::to_stdvec(&challenge).map_err(|e|invalid(&e.to_string()))?;
        if metadata.is_empty() || metadata.len()>MAX_CHALLENGE_BYTES
            || row.is_empty() || row.len()>MAX_CREATION_ROW_BYTES {return Err(invalid("Private creation response exceeds its frame bounds"));}
        let mut header=[0u8;12];header[..4].copy_from_slice(&MAGIC);
        header[4..8].copy_from_slice(&(metadata.len() as u32).to_be_bytes());
        header[8..12].copy_from_slice(&(row.len() as u32).to_be_bytes());
        write_all(stream,&header,deadline,cancellation)?;write_all(stream,&metadata,deadline,cancellation)?;write_all(stream,&row,deadline,cancellation)?;
        created_root_rights::send_cancellable(stream,files,deadline,cancellation)
    })();
    fail_closed(stream,result)
}

/// Host consumes EXACT private response bytes before calling recvmsg for the
/// rights phase. No FrameCodec read can cross into the ancillary byte.
pub(crate) fn receive_response(stream:&UnixStream,deadline:Instant)
    ->io::Result<(UiJournalResponse,created_root_rights::Roots)> {
    receive_response_cancellable(stream,deadline,&crate::process::CancellationToken::new())
}
pub(crate) fn receive_response_cancellable(stream:&UnixStream,deadline:Instant,
    cancellation:&crate::process::CancellationToken)->io::Result<(UiJournalResponse,created_root_rights::Roots)> {
    let result=(|| {
        let mut header=[0u8;12];read_exact(stream,&mut header,deadline,cancellation)?;
        let metadata_len=u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
        let row_len=u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
        // Check BOTH lengths before allocating either buffer.
        if header[..4]!=MAGIC || metadata_len==0 || metadata_len>MAX_CHALLENGE_BYTES
            || row_len==0 || row_len>MAX_CREATION_ROW_BYTES {return Err(invalid("Invalid private creation response header"));}
        let mut metadata=vec![0;metadata_len];read_exact(stream,&mut metadata,deadline,cancellation)?;
        let (challenge,unused): (UiJournalChallenge, &[u8]) = postcard::take_from_bytes(&metadata)
            .map_err(|e|invalid(&e.to_string()))?;
        if !unused.is_empty() {return Err(invalid("Trailing private creation challenge metadata"));}
        challenge.validate_shape()?;
        let mut row=vec![0;row_len];read_exact(stream,&mut row,deadline,cancellation)?;
        let response=UiJournalResponse::from_wire(challenge,row)?;
        let roots=created_root_rights::receive_cancellable(stream,deadline,cancellation)?;
        Ok((response,roots))
    })();
    fail_closed(stream,result)
}
fn fail_closed<T>(stream:&UnixStream,result:io::Result<T>)->io::Result<T> {
    if result.is_err() {let _=stream.shutdown(std::net::Shutdown::Both);}
    // shutdown prevents further protocol use. It is NOT descriptor settlement:
    // the caller must drop original +writer clones to release queued rights.
    result
}
fn write_all(stream:&UnixStream,mut bytes:&[u8],deadline:Instant,cancellation:&crate::process::CancellationToken)->io::Result<()> {
    while !bytes.is_empty() {
        check(deadline,cancellation)?;
        let sent=unsafe{libc::send(stream.as_raw_fd(),bytes.as_ptr().cast(),bytes.len(),libc::MSG_DONTWAIT|libc::MSG_NOSIGNAL)};
        if sent>0 {bytes=&bytes[sent as usize..];continue;}
        if sent==0 {return Err(io::ErrorKind::WriteZero.into());}
        let error=io::Error::last_os_error();match error.kind(){
            io::ErrorKind::Interrupted=>{},io::ErrorKind::WouldBlock=>wait(stream,libc::POLLOUT,deadline,cancellation)?,_=>return Err(error)}
    }Ok(())
}
fn read_exact(stream:&UnixStream,mut bytes:&mut[u8],deadline:Instant,cancellation:&crate::process::CancellationToken)->io::Result<()> {
    while !bytes.is_empty() {
        check(deadline,cancellation)?;
        let read=unsafe{libc::recv(stream.as_raw_fd(),bytes.as_mut_ptr().cast(),bytes.len(),libc::MSG_DONTWAIT)};
        if read>0 {bytes=&mut bytes[read as usize..];continue;}
        if read==0 {return Err(io::ErrorKind::UnexpectedEof.into());}
        let error=io::Error::last_os_error();match error.kind(){
            io::ErrorKind::Interrupted=>{},io::ErrorKind::WouldBlock=>wait(stream,libc::POLLIN,deadline,cancellation)?,_=>return Err(error)}
    }Ok(())
}
fn wait(stream:&UnixStream,events:libc::c_short,deadline:Instant,cancellation:&crate::process::CancellationToken)->io::Result<()> {
    loop {check(deadline,cancellation)?;let remaining=deadline.checked_duration_since(Instant::now()).filter(|time|!time.is_zero())
        .ok_or_else(||io::Error::from(io::ErrorKind::TimedOut))?;
        let millis=remaining.as_millis().saturating_add(1).min(50) as i32;
        let mut fd=libc::pollfd{fd:stream.as_raw_fd(),events,revents:0};
        let result=unsafe{libc::poll(&mut fd,1,millis)};
        if result>0 {return Ok(());}if result==0 {continue;}
        let error=io::Error::last_os_error();if error.kind()!=io::ErrorKind::Interrupted {return Err(error);}
    }
}
fn check(deadline:Instant,cancellation:&crate::process::CancellationToken)->io::Result<()> {
    if cancellation.is_cancelled() {return Err(io::ErrorKind::Interrupted.into());}
    if Instant::now()>=deadline {Err(io::ErrorKind::TimedOut.into())}else{Ok(())}
}
fn invalid(message:&str)->io::Error {io::Error::new(io::ErrorKind::InvalidData,message)}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::{Read,Write},os::fd::AsFd,time::Duration};
    fn challenge()->UiJournalChallenge {
        UiJournalChallenge {request_id:9,nonce:crate::model::OperationId::new(),
            proof:crate::model::HostSessionProof{host_session_id:"host".into(),boot_id:"boot".into(),host_incarnation:"incarnation".into()},
            requirement:crate::model::DirectoryStartRequirement {card:crate::model::CardId::new(),session:crate::model::TerminalId::new(),
                operation:crate::model::OperationId::new(),generation:7,attempt:uuid::Uuid::new_v4(),kind:crate::model::DirectoryStartKind::Terminal},
            window:"primary".into(),database:[1,2],roles:[[3,4];6]}
    }
    #[test]
    fn private_response_preserves_six_distinct_original_roots_and_next_frame() {
        use std::os::unix::fs::MetadataExt;
        let temp=tempfile::tempdir().unwrap();
        let roots:[crate::fs_safety::Root;6]=std::array::from_fn(|index| {
            let name=temp.path().join(format!("original-{index}"));std::fs::create_dir(&name).unwrap();
            std::fs::write(name.join("marker"),format!("original {index}")).unwrap();crate::fs_safety::Root::open(&name).unwrap()
        });
        let files=roots.each_ref().map(|root|root.retained_directory().unwrap());
        let refs=files.each_ref().map(|file|file.as_fd());
        let mut expected=challenge();expected.roles=roots.each_ref().map(|root|{let m=root.metadata().unwrap();[m.dev(),m.ino()]});
        let row=b"bounded original connection evidence".to_vec();
        let response=UiJournalResponse::from_wire(expected.clone(),row.clone()).unwrap();
        let (a,mut b)=UnixStream::pair().unwrap();let deadline=Instant::now()+Duration::from_secs(2);
        send_response(&a,response,&refs,deadline).unwrap();
        crate::terminal_ipc::write_frame(&mut &a,&42u32).unwrap();
        for (index,root) in roots.iter().enumerate() {
            let name=root.path();std::fs::rename(name,temp.path().join(format!("parked-{index}"))).unwrap();
            std::fs::create_dir(name).unwrap();std::fs::write(name.join("marker"),b"replacement").unwrap();
        }
        let (response,received)=receive_response(&b,deadline).unwrap();
        let (actual,actual_row)=response.into_wire();assert_eq!(actual,expected);assert_eq!(actual_row,row);
        for (index,file) in received.0.into_iter().enumerate() {
            let m=file.metadata().unwrap();assert_eq!([m.dev(),m.ino()],expected.roles[index]);
            let pin=crate::fs_safety::Root::from_received_creation_directory(file.into(),roots[index].path().to_owned(),
                (m.dev(),m.ino())).unwrap();
            let mut marker=String::new();crate::fs_safety::open_beneath(&pin,"marker".as_ref()).unwrap().read_to_string(&mut marker).unwrap();
            assert_eq!(marker,format!("original {index}"));
        }
        assert_eq!(crate::terminal_ipc::read_frame::<u32>(&mut b).unwrap(),Some(42));
    }
    #[test]
    fn private_response_refuses_both_unbounded_prefixes_before_body_or_rights() {
        for (metadata,row) in [(MAX_CHALLENGE_BYTES as u32+1,1),(1,MAX_CREATION_ROW_BYTES as u32+1)] {
            let (mut a,b)=UnixStream::pair().unwrap();let mut header=[0u8;12];header[..4].copy_from_slice(&MAGIC);
            header[4..8].copy_from_slice(&metadata.to_be_bytes());header[8..12].copy_from_slice(&row.to_be_bytes());
            a.write_all(&header).unwrap();assert!(receive_response(&b,Instant::now()+Duration::from_secs(1)).is_err());
            a.set_read_timeout(Some(Duration::from_secs(1))).unwrap();assert_eq!(a.read(&mut[0u8;1]).unwrap(),0);
        }
    }
    #[test]
    fn private_response_refuses_metadata_suffix_before_reading_row_or_rights() {
        let mut metadata = postcard::to_stdvec(&challenge()).unwrap();
        metadata.push(0);
        let (mut a, b) = UnixStream::pair().unwrap();
        let mut header = [0u8; 12];
        header[..4].copy_from_slice(&MAGIC);
        header[4..8].copy_from_slice(&(metadata.len() as u32).to_be_bytes());
        header[8..12].copy_from_slice(&1u32.to_be_bytes());
        a.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
        a.write_all(&header).unwrap();
        a.write_all(&metadata).unwrap();
        let error = receive_response(&b, Instant::now() + Duration::from_secs(1)).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        a.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(a.read(&mut [0u8; 1]).unwrap(), 0);
    }
    #[test]
    fn cancellation_interrupts_partial_header_and_pending_rights_with_one_budget() {
        let metadata = postcard::to_stdvec(&challenge()).unwrap();
        let mut through_row = Vec::new();
        through_row.extend_from_slice(&MAGIC);
        through_row.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
        through_row.extend_from_slice(&1u32.to_be_bytes());
        through_row.extend_from_slice(&metadata);
        through_row.push(1);
        for prefix in [b"CJR".as_slice(), through_row.as_slice()] {
            let (mut sender, receiver) = UnixStream::pair().unwrap();
            sender.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
            sender.write_all(prefix).unwrap();
            let cancellation = crate::process::CancellationToken::new();
            let before = Instant::now();
            std::thread::scope(|scope| {
                let (done, result) = std::sync::mpsc::sync_channel(1);
                let token = &cancellation;
                let worker = scope.spawn(move || {
                    let error = receive_response_cancellable(&receiver,
                        before + Duration::from_secs(2), token).err().unwrap();
                    let _ = done.send(error.kind());
                });
                std::thread::sleep(Duration::from_millis(20));
                cancellation.cancel();
                let observed = result.recv_timeout(Duration::from_millis(500));
                // Even a failed assertion must close the peer and settle the
                // bounded original worker, rather than leave a blocked reader.
                let _ = sender.shutdown(std::net::Shutdown::Both);
                worker.join().unwrap();
                assert_eq!(observed.unwrap(), io::ErrorKind::Interrupted);
            });
            assert!(before.elapsed() < Duration::from_secs(1));
        }
    }
    #[test]
    fn incomplete_private_frame_uses_one_deadline_and_invalidates_stream() {
        let (mut a,b)=UnixStream::pair().unwrap();a.write_all(b"CJR").unwrap();
        let before=Instant::now();let result=receive_response(&b,before+Duration::from_millis(50));
        assert_eq!(result.err().unwrap().kind(),io::ErrorKind::TimedOut);
        assert!(before.elapsed()<Duration::from_secs(1));
        a.set_read_timeout(Some(Duration::from_secs(1))).unwrap();assert_eq!(a.read(&mut[0u8;1]).unwrap(),0);
    }
}
