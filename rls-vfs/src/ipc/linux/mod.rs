#[macro_use]
mod error;

pub use error::{LibcError, RlsVfsIpcError};

use super::*;
use std::sync::Arc;
use std::clone::Clone;
use std::rc::{Rc, Weak};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use mio::{Poll, Token};
use serde::{Serialize, de::DeserializeOwned};

pub type Result<T> = std::result::Result<T, RlsVfsIpcError>;
pub type LibcResult<T> = std::result::Result<T, LibcError>;

// A wrapper around linux fd which requires you to explicitly close it, Fd won't close itself on drop but panic, so remember to close it
pub enum Fd {
    Closed,
    Open(libc::c_int),
}

impl Fd {
    pub fn from_raw(fd: libc::c_int) -> Self {
        Fd::Open(fd)
    }

    pub fn close(&mut self) -> LibcResult<()> {
        match self {
            Fd::Closed => {
                // fake a libc error of invalid fd, otherwise would complicate our error hierarchy
                fake_libc_error!("close", libc::EBADF);
            },
            Fd::Open(fd) => {
                let res = unsafe {
                    libc::close(*fd)
                };
                if res < 0 {
                    handle_libc_error!("close");
                } else {
                    std::mem::forget(std::mem::replace(self, Fd::Closed));
                }
            }
        }
        Ok(())
    }

    pub fn try_close(&mut self) -> LibcResult<()> {
        match self {
            Fd::Closed => {
                Ok(())
            },
            Fd::Open(fd) => {
                self.close()
            }
        }
    }

    pub fn try_clone(&self) -> LibcResult<Fd> {
        match *self {
            Fd::Closed => {
                Ok(Fd::Closed)
            },
            Fd::Open(fd) => {
                let fd1 = unsafe { libc::dup(fd) };
                if fd1 < 0 {
                    handle_libc_error!("dup");
                }
                Ok(Fd::Open(fd1))
            }
        }
    }

    pub fn get_fd(&self) -> LibcResult<libc::c_int> {
        match self {
            Fd::Closed => {
                fake_libc_error!("Fd::get_fd", libc::EBADF);
            }
            Fd::Open(fd) => {
                Ok(*fd)
            }
        }
    }

    // take the responsibility of closing the fd
    pub fn take_raw(&mut self) -> LibcResult<libc::c_int> {
        match self {
            Fd::Open(fd) => {
                let fd = *fd;
                std::mem::forget(std::mem::replace(self, Fd::Closed));
                Ok(fd)
            },
            Fd::Closed => {
                fake_libc_error!("Fd::take_raw", libc::EBADF);
            }
        }
    }

    pub fn write(&self, cont: &[u8]) -> LibcResult<usize> {
        let len = cont.len();
        let res = unsafe { libc::write(self.get_fd()?, &cont[0] as *const u8 as *const libc::c_void, len) };
        if res < 0 {
            handle_libc_error!("write");
        }
        Ok(res as usize)
    }

    pub fn read(&self, buf: &mut [u8]) -> LibcResult<usize> {
        let len = buf.len();
        let res = unsafe { libc::read(self.get_fd()?, &mut buf[0] as *mut u8 as *mut libc::c_void, len) };
        if res < 0 {
            handle_libc_error!("write");
        }
        Ok(res as usize)
    }

    pub fn write_all(&self, cont:&[u8]) -> LibcResult<()> {
        let len = cont.len();
        let mut start_pos = 0;
        let write_fd = self.get_fd()?;
        while start_pos < len {
            let res = unsafe { libc::write(write_fd, &cont[start_pos] as *const u8 as *const libc::c_void, len - start_pos) };
            if res <= 0 {
                handle_libc_error!("write");
            }
            start_pos += res as usize;
        }
        Ok(())
    }

    pub fn read_all(&self, buf: &mut [u8]) -> LibcResult<()> {
        let len = buf.len();
        let mut start_pos = 0;
        let read_fd = self.get_fd()?;
        while start_pos < len {
            let res = unsafe { libc::read(read_fd, &mut buf[start_pos] as *mut u8 as *mut libc::c_void, len - start_pos) };
            if res <= 0 {
                handle_libc_error!("write");
            }
            start_pos += res as usize;
        }
        Ok(())
    }

    pub fn read_till_close(&self) -> LibcResult<Vec<u8>> {
        let mut buf: [u8;4096] = unsafe { std::mem::uninitialized() };
        let mut ret = Vec::new();
        let read_fd = self.get_fd()?;
        loop {
            let res = unsafe { libc::read(read_fd, &mut buf[0] as *mut u8 as *mut libc::c_void, std::mem::size_of_val(&buf)) };
            if res < 0 {
                handle_libc_error!("write");
            }
            if res == 0 {
                break;
            }
            ret.extend_from_slice(&buf[0..(res as usize)]);
        }
        Ok(ret)
    }

    pub fn make_nonblocking() -> LibcResult<()> {
        unimplemented!()
    }

    pub fn make_blocking() -> LibcResult<()> {
        unimplemented!()
    }

    pub fn is_nonblocking() -> LibcResult<bool>{
        unimplemented!()
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        match self {
            Fd::Open(_) => {
                panic!("you forget to close a fd before it is dropped");
            }
            Fd::Closed => ()
        }
    }
}

#[cfg(test)]
mod tests_fd {
    use super::*;
    use std::io::stderr;
    use std::io::Write;

    #[test]
    #[should_panic]
    fn unclosed_fd_panic() {
        let fd = Fd::from_raw(1);
    }

    #[test]
    fn double_close_error() {
        let mut fds:[libc::c_int;2] = unsafe {std::mem::uninitialized()};
        assert!(unsafe { libc::pipe2(&mut fds[0] as *mut libc::c_int, 0) } == 0);
        let mut fd1 = Fd::from_raw(fds[0]);
        let mut fd2 = Fd::from_raw(fds[1]);
        assert!(fd1.close().is_ok());
        assert!(fd2.close().is_ok());
        assert!(fd1.close().is_err());
        assert!(fd2.close().is_err());
    }

    #[test]
    fn closed_fd_not_panic() {
        let mut fds:[libc::c_int;2] = unsafe {std::mem::uninitialized()};
        assert!(unsafe { libc::pipe2(&mut fds[0] as *mut libc::c_int, 0) } == 0);
        let mut fd1 = Fd::from_raw(fds[0]);
        let mut fd2 = Fd::from_raw(fds[1]);
        let mut fd3 = fd2;
        assert!(fd1.close().is_ok());
        assert!(fd3.close().is_ok());
    }

    #[test]
    fn close_invalid_fd_error() {
        // I hope this is a invalid fd
        let mut fd = Fd::from_raw(-1 as libc::c_int);
        assert!(fd.close().is_err());
        fd.take_raw().unwrap();
    }
}

// a wrapper around linux pipe fd which requires you to explicitly close it
struct Pipe {
    read_fd: Fd,
    write_fd: Fd,
}

impl Pipe {
    pub fn new() -> LibcResult<Pipe> {
        let mut fds: [libc::c_int;2] = unsafe {std::mem::uninitialized() };
         let res = unsafe {
            libc::pipe2(&mut fds[0] as *mut libc::c_int, 0)
         };
         if res < 0 {
             handle_libc_error!("pipe2");
         }
         Ok(Pipe {
             read_fd: Fd::from_raw(fds[0]),
             write_fd: Fd::from_raw(fds[1]),
         })
    }

    pub fn close_write(&mut self) -> LibcResult<()> {
        self.write_fd.close()
    }

    pub fn close_read(&mut self) -> LibcResult<()> {
        self.read_fd.close()
    }

    pub fn close(&mut self) -> LibcResult<()> {
        self.close_write()?;
        self.close_read()
    }

    pub fn try_close(&mut self) -> LibcResult<()> {
        self.write_fd.try_close()?;
        self.read_fd.try_close()
    }

    pub fn write(&self, cont: &[u8]) -> LibcResult<usize> {
        self.write_fd.write(cont)
    }

    pub fn read(&self, buf: &mut [u8]) -> LibcResult<usize> {
        self.read_fd.read(buf)
    }

    pub fn write_all(&self, cont:&[u8]) -> LibcResult<()> {
        self.write_fd.write_all(cont)
    }

    pub fn read_all(&self, buf: &mut [u8]) -> LibcResult<()> {
        self.read_fd.read_all(buf)
    }

    pub fn read_till_close(&self) -> LibcResult<Vec<u8>> {
        self.read_fd.read_till_close()
    }

    pub fn take_read(&mut self) -> Fd {
        std::mem::replace(&mut self.read_fd, Fd::Closed)
    }

    pub fn take_write(&mut self) -> Fd {
        std::mem::replace(&mut self.write_fd, Fd::Closed)
    }

}

#[cfg(test)]
mod tests_pipe {
    use super::*;
    struct ChildProcess(pub libc::c_int);

    #[test]
    fn pipe_new_close() {
        let mut pipe = Pipe::new().unwrap();
        pipe.close().unwrap();
    }

    #[test]
    #[should_panic]
    fn pipe_new_no_close() {
        // FIXME: test for double panic(abort)
        let mut pipe = Pipe::new().unwrap();
        pipe.close_read().unwrap();
    }

    fn prop_read_write(pipe:&Pipe, input: &[u8]) -> bool {
        let len = input.len();
        let mut buf = Vec::<u8>::with_capacity(len);
        pipe.write_all(input).unwrap();
        buf.resize(len, 0u8);
        pipe.read_all(&mut buf).unwrap();
        input == buf.as_slice()
    }

    #[quickcheck]
    fn check_write_read(input: Vec<u8>) -> bool {
        // TODO: large size pipe write/read, blocking test
        eprintln!("input size {}", input.len());
        let mut pipe = Pipe::new().unwrap();
        let ret = prop_read_write(&pipe, &input);
        pipe.close().unwrap();
        ret
    }

    #[quickcheck]
    fn threaded_write_read(input: Vec<u8>) -> bool {
        // TODO: large size pipe write/read, blocking test
        eprintln!("input size {}", input.len());
        let mut pipe = Pipe::new().unwrap();
        let mut read_fd = pipe.take_read();
        let mut write_fd = pipe.take_write();
        let input1 = input.clone();
        let t1 = std::thread::spawn(move ||{
            write_fd.write_all(&input1).unwrap();
            write_fd.close().unwrap();
        });
        let res = read_fd.read_till_close().unwrap();
        read_fd.close().unwrap();
        let ret = res == input;
        t1.join().unwrap();
        ret
    }

    #[quickcheck]
    fn inter_process_write_read(input: Vec<u8>) -> bool {
        let test = || {
        // TODO: large size pipe write/read, blocking test
        eprintln!("input size {}", input.len());
        let mut pipe = Pipe::new().unwrap();
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            // fork failed
            (-1, false)
        } else if pid == 0 {
            // child process
            pipe.write_all(&input).unwrap();
            pipe.close().unwrap();
            (pid, true)
        } else {
            pipe.close_write().unwrap();
            let res = pipe.read_till_close().unwrap();
            pipe.close_read().unwrap();
            (-1, res == input)
        }
        };
        let (pid, res) = test();
        if pid >= 0 {
            std::process::exit(0);
        } else {
            res
        }
    }
}

pub struct LinuxVfsIpcChannel {
    s2c_pipe: Pipe,
    c2s_pipe: Pipe,
}

impl VfsIpcChannel for LinuxVfsIpcChannel {
    type ServerEndPoint = LinuxVfsIpcServerEndPoint;
    type ClientEndPoint = LinuxVfsIpcClientEndPoint;
    type Error = LibcError;

    fn new_prefork() -> LibcResult<Self> {
        Ok( LinuxVfsIpcChannel {
            s2c_pipe: Pipe::new()?,
            c2s_pipe: Pipe::new()?,
        }
    }

    fn into_server_end_point_postfork(mut self) -> LibcResult<Self::ServerEndPoint> {
        /*
        eprintln!("server pipe fd {} {}", self.s2c_pipe.write_fd.get_fd()?, self.s2c_pipe.read_fd.get_fd()?);
        eprintln!("server pipe fd {} {}", self.c2s_pipe.write_fd.get_fd()?, self.c2s_pipe.read_fd.get_fd()?);
        eprintln!("server close fd {} {}", self.s2c_pipe.read_fd.get_fd()?, self.c2s_pipe.write_fd.get_fd()?);
        eprintln!("server write fd: {}", self.s2c_pipe.write_fd.get_fd()?);
        eprintln!("server read fd: {}", self.c2s_pipe.read_fd.get_fd()?);
        */
        self.s2c_pipe.close_read()?;
        self.c2s_pipe.close_write()?;
        Self::ServerEndPoint::new(self.c2s_pipe.read_fd, self.s2c_pipe.write_fd)
    }

    fn into_client_end_point_postfork(mut self) -> LibcResult<Self::ClientEndPoint> {
        /*
        eprintln!("client pipe fd {} {}", self.s2c_pipe.write_fd.get_fd()?, self.s2c_pipe.read_fd.get_fd()?);
        eprintln!("client pipe fd {} {}", self.c2s_pipe.write_fd.get_fd()?, self.c2s_pipe.read_fd.get_fd()?);
        eprintln!("client close fd {} {}", self.s2c_pipe.write_fd.get_fd()?, self.c2s_pipe.read_fd.get_fd()?);
        eprintln!("client write fd: {}", self.c2s_pipe.write_fd.get_fd()?);
        eprintln!("client read fd: {}", self.s2c_pipe.read_fd.get_fd()?);
        */
        self.s2c_pipe.close_write()?;
        self.c2s_pipe.close_read()?;
        Self::ClientEndPoint::new(self.s2c_pipe.read_fd, self.c2s_pipe.write_fd)
    }
}

impl LinuxVfsIpcChannel {
    pub fn take(&mut self) -> LinuxVfsIpcChannel {
        let closed = LinuxVfsIpcChannel {
            s2c_pipe: Pipe {
                read_fd: Fd::Closed,
                write_fd: Fd::Closed,
            },
            c2s_pipe: Pipe {
                read_fd: Fd::Closed,
                write_fd: Fd::Closed,
            },
        };
        std::mem::replace(self, closed)
    }

    pub fn close(&mut self) -> LibcResult<()> {
        self.s2c_pipe.close()?;
        self.c2s_pipe.close()
    }
}

fn blocking_read_impl<T: Serialize + DeserializeOwned + Clone>(read_fd: &Fd, rbuf: &mut Vec<u8>) -> Result<T> {
    let mut buf1:[u8;4096] = unsafe {std::mem::uninitialized()};
    let read_fd = read_fd.get_fd()?;
    macro_rules! read_and_append {
        () => {
            let res = unsafe {
                libc::read(read_fd, &mut buf1[0] as *mut u8 as *mut libc::c_void, std::mem::size_of_val(&buf1))
            };
            if res < 0 {
            // NB: no need to handle EWOULDBLOCK, as client side is blocking fd
            // TODO: more fine grained error handling, like interrupted by a signal
                handle_libc_error!("read");
            }
            rbuf.extend_from_slice(&buf1[..res as usize]);
        }
    }

    while rbuf.len() < 4 {
        read_and_append!();
    }

    let len = match bincode::deserialize::<u32>(&rbuf[..4]) {
        Ok(len) => len as usize + 4,
        Err(err) => {
            return Err(RlsVfsIpcError::DeserializeError(err));
        },
    };
    while rbuf.len() < len {
        read_and_append!();
    }
    let msg:T = match bincode::deserialize(&rbuf[4..len]) {
        Ok(msg) => msg,
        Err(err) => {
            return Err(RlsVfsIpcError::DeserializeError(err));
        },
    };
    *rbuf = rbuf.split_off(len);
    Ok(msg)
}

fn blocking_write_impl<T: Serialize + DeserializeOwned + Clone>(write_fd: &Fd, t: &T, wbuf: &mut Vec<u8>) -> Result<()> {
    let mut ext2 = match bincode::serialize(t) {
        Ok(ext) => ext,
        Err(err) => {
            return Err(RlsVfsIpcError::SerializeError(err));
        },
    };
    let len = ext2.len() as u32;
    let mut ext1 = match bincode::serialize(&len) {
        Ok(ext) => ext,
        Err(err) => {
            return Err(RlsVfsIpcError::SerializeError(err));
        },
    };
    wbuf.reserve(wbuf.len() + ext1.len() + ext2.len());
    wbuf.append(&mut ext1);
    wbuf.append(&mut ext2);
    write_fd.write_all(&wbuf)?;
    wbuf.clear();
    Ok(())
}

pub struct LinuxVfsIpcClientEndPoint {
    read_fd: Fd,
    write_fd: Fd,
}

impl LinuxVfsIpcClientEndPoint {
    pub fn new(read_fd: Fd, write_fd: Fd) -> LibcResult<Self> {
        Ok(Self {
            read_fd,
            write_fd,
        })
    }

    pub fn close(&mut self) -> LibcResult<()> {
        self.write_fd.close()?;
        self.read_fd.close()
    }

    fn write_request(&mut self, req_msg: VfsRequestMsg) -> Result<()> {
        let buf = match bincode::serialize(&req_msg) {
            Ok(buf) => buf,
            Err(err) => {
                return Err(RlsVfsIpcError::SerializeError(err));
            }
        };
        let len = buf.len();
        let mut start_pos = 0;
        let write_fd = self.write_fd.get_fd()?;
        while start_pos < len {
            let res = unsafe {
                libc::write(write_fd, &buf[start_pos] as *const u8 as *const libc::c_void, len - start_pos)
            };
            if res < 0 {
                // NB: no need to handle EWOULDBLOCK, as client side is blocking fd
                // TODO: more fine grained error handling, like interrupted by a signal
                handle_libc_error!("write");
            }
            start_pos += res as usize;
        }
        Ok(())
    }

    fn read_reply<U: Serialize + DeserializeOwned + Clone>(&mut self) -> Result<VfsReplyMsg<U>> {
        let mut buf1:[u8;4096] = unsafe {std::mem::uninitialized()};
        let mut buf = Vec::<u8>::new();
        let read_fd = self.read_fd.get_fd()?;
        macro_rules! read_and_append {
            () => {
                let res = unsafe {
                    libc::read(read_fd, &mut buf1[0] as *mut u8 as *mut libc::c_void, std::mem::size_of_val(&buf1))
                };
                if res < 0 {
                // NB: no need to handle EWOULDBLOCK, as client side is blocking fd
                // TODO: more fine grained error handling, like interrupted by a signal
                    handle_libc_error!("read");
                }
            }
        }
        loop {
            read_and_append!();
            if buf.len() >= 4 {
                break;
            }
        }
        let len = match bincode::deserialize::<u32>(&buf[0..4]) {
            Ok(len) => len as usize,
            Err(err) => {
                return Err(RlsVfsIpcError::DeserializeError(err));
            },
        };
        buf.reserve(len);
        while buf.len() < len {
            read_and_append!();
        }
        match bincode::deserialize(&buf[4..len]) {
            Ok(ret) => {
                Ok(ret)
            },
            Err(err) => {
                Err(RlsVfsIpcError::DeserializeError(err))
            }
        }
    }
}

impl VfsIpcClientEndPoint for LinuxVfsIpcClientEndPoint {
    type Error = RlsVfsIpcError;
    type FileHandle = LinuxVfsIpcFileHandle;
    type ReadBuffer = Vec<u8>;
    type WriteBuffer = Vec<u8>;

    fn blocking_write_request(&mut self, req:&VfsRequestMsg, wbuf: &mut Self::WriteBuffer) -> Result<()> {
        blocking_write_impl(&self.write_fd, req, wbuf)
    }

    fn blocking_read_reply<U: Serialize + DeserializeOwned + Clone>(&mut self, rbuf: &mut Self::ReadBuffer) -> Result<VfsReplyMsg<U>> {
        blocking_read_impl(&self.read_fd, rbuf)
    }

    fn reply_to_file_handle<U: Serialize + DeserializeOwned + Clone>(&mut self, rep: &VfsReplyMsg<U>) -> Result<Self::FileHandle> {
        unimplemented!()
    }
}

pub struct LinuxVfsIpcServerEndPoint {
    read_fd: Fd,
    write_fd: Fd,
}

impl LinuxVfsIpcServerEndPoint {
    fn new(read_fd: Fd, write_fd: Fd) -> LibcResult<Self> {
        let r_fd = match read_fd {
            Fd::Open(fd) => {
                fd
            },
            Fd::Closed => {
                fake_libc_error!("LinuxVfsIpcServerEndPoint::new", libc::EBADF);
            }
        };
        let w_fd = match write_fd {
            Fd::Open(fd) => {
                fd
            },
            Fd::Closed => {
                fake_libc_error!("LinuxVfsIpcServerEndPoint::new", libc::EBADF);
            }
        };
        unsafe {
            if libc::fcntl(r_fd, libc::F_SETFL, libc::O_NONBLOCK) < 0 ||  libc::fcntl(w_fd, libc::F_SETFL, libc::O_NONBLOCK) < 0 {
                handle_libc_error!("fcntl");
            }
        }
        Ok(Self {
            read_fd,
            write_fd,
        })
    }

    pub fn close(&mut self) -> LibcResult<()> {
        self.write_fd.close()?;
        self.read_fd.close()
    }
}

#[cfg(test)]
mod test_end_points {
    use super::*;
    use rand::Rng;

    fn end_points_new_1(should_close: bool) {
        let channel = LinuxVfsIpcChannel::new_prefork().unwrap();
        let res = unsafe { libc::fork() };
        if res < 0 {
            panic!("failed to fork");
        } else if res == 0 {
            // child process
            let mut ep = channel.into_client_end_point_postfork().unwrap();
            if should_close {
                ep.close().unwrap();
            } else {
                ep.write_fd.close().unwrap();
            }
        } else {
            // parent process
            let mut ep = channel.into_server_end_point_postfork().unwrap();
            if should_close {
                ep.close().unwrap();
            } else {
                ep.write_fd.close().unwrap();
            }
            let res = unsafe { libc::kill(res, libc::SIGKILL) };
            if res < 0 {
                panic!("failed to kill child process");
            }
        }
    }

    #[test]
    fn end_points_new_close() {
        end_points_new_1(true);
    }

    #[test]
    #[should_panic]
    fn end_points_new_no_close() {
        end_points_new_1(false);
    }

    const PATH_COMP:usize = 100;
    const PATH_LEN:usize = 100;
    const MSG_MAX:usize = 100;
    const STR_MAX:usize = 100;
    const CONT_MAX:u32 = 100_00;

    fn random_ascii_string(min_len:usize, max_len:usize) -> String {
        let mut rng = rand::thread_rng();
        let char_dist = rand::distributions::Uniform::<u8>::new_inclusive(1, 127);
        let len_dist = rand::distributions::Uniform::<usize>::new_inclusive(min_len, max_len);
        let str_len = rng.sample(&len_dist);
        rng.sample_iter(&char_dist).take(str_len).map(|c| { c as char }).collect::<String>()
    }

    fn generate_random_request() -> VfsRequestMsg {
        let mut rng = rand::thread_rng();
        let comp_dist = rand::distributions::Uniform::<usize>::new_inclusive(1, PATH_COMP);
        let path_comp = rng.sample(comp_dist);
        let mut path = PathBuf::new();
        for p in 0..path_comp {
            path.push(random_ascii_string(1, PATH_LEN));
        }
        if rng.gen::<bool>() {
            VfsRequestMsg::OpenFile(path)
        } else {
            VfsRequestMsg::CloseFile(path)
        }

    }

    fn generate_random_reply() -> VfsReplyMsg<String> {
        let mut rng = rand::thread_rng();
        let user_data = random_ascii_string(1, STR_MAX);
        let path = random_ascii_string(1, PATH_LEN);
        let length_dist = rand::distributions::Uniform::<u32>::new_inclusive(0 as u32, CONT_MAX);
        let length = rng.sample(&length_dist);
        VfsReplyMsg::<String> {
            path,
            length,
            user_data,
        }
    }

    fn prepare_request_reply() -> Vec<(VfsRequestMsg, VfsReplyMsg<String>)> {
        let mut rng = rand::thread_rng();
        let msg_dist = rand::distributions::Uniform::<usize>::new_inclusive(1, MSG_MAX);
        let msg_num = rng.sample(&msg_dist);
        let mut ret = Vec::with_capacity(msg_num);
        for n  in 0..msg_num {
            ret.push((generate_random_request(), generate_random_reply()));
        }
        ret
    }

    enum ReqRep {
        Parent(Vec<libc::c_int>, Vec<LinuxVfsIpcServerEndPoint>, Vec<Vec<(VfsRequestMsg, VfsReplyMsg<String>)>>),
        Children(i32, LinuxVfsIpcClientEndPoint, Vec<(VfsRequestMsg, VfsReplyMsg<String>)>),
    }

    fn prepare_fork(children_num: usize) -> ReqRep {
        let mut req_reps = Vec::with_capacity(children_num);
        let mut eps:Vec<LinuxVfsIpcServerEndPoint> = Vec::with_capacity(children_num);
        let mut pids = Vec::with_capacity(children_num);
        for n in 0..children_num {
            let req_rep = prepare_request_reply();
            let channel = LinuxVfsIpcChannel::new_prefork().unwrap();
            let res = unsafe { libc::fork() };
            if res < 0 {
                panic!("failed to fork");
            } else if res == 0 {
                for p in eps.iter_mut() {
                    p.close().unwrap();
                }
                return ReqRep::Children(res, channel.into_client_end_point_postfork().unwrap(), req_rep);
            } else {
                pids.push(res);
                let ep = channel.into_server_end_point_postfork().unwrap();
                eps.push(ep);
                req_reps.push(req_rep);
            }
        }
        return ReqRep::Parent(pids, eps, req_reps);
    }

    // server side
    fn request_reply_server(ep: &mut LinuxVfsIpcServerEndPoint, req_rep: &Vec<(VfsRequestMsg, VfsReplyMsg<String>)>) -> bool {
        let mut buf1:[u8;4096] = unsafe {std::mem::uninitialized()};
        let mut buf = Vec::<u8>::new();
        let read_fd = ep.read_fd.get_fd().unwrap();
        let write_fd = ep.read_fd.get_fd().unwrap();
        // temporarily set the server end point blocking
        unsafe {
            let flags = libc::fcntl(read_fd, libc::F_GETFL, 0);
            libc::fcntl(read_fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);

            let flags = libc::fcntl(write_fd, libc::F_GETFL, 0);
            libc::fcntl(write_fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }

        let mut rbuf = Vec::<u8>::new();
        let mut wbuf = Vec::<u8>::new();
        for (req, rep) in req_rep {
            let msg = ep.blocking_read_request(&mut rbuf).unwrap();
            if msg != *req {
                return false;
            }
            ep.blocking_write_reply(&rep, &mut wbuf).unwrap();
        }

        unsafe {
            let flags = libc::fcntl(read_fd, libc::F_GETFL, 0);
            libc::fcntl(read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);

            let flags = libc::fcntl(write_fd, libc::F_GETFL, 0);
            libc::fcntl(write_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        return rbuf.is_empty() && wbuf.is_empty();
    }

    fn request_reply_client(ep: &mut LinuxVfsIpcClientEndPoint, req_rep: &Vec<(VfsRequestMsg, VfsReplyMsg<String>)>) -> bool {
        let mut rbuf = Vec::<u8>::new();
        let mut wbuf = Vec::<u8>::new();
        for (req, rep) in req_rep {
            ep.blocking_write_request(&req, &mut wbuf);
            let msg = ep.blocking_read_reply(&mut rbuf).unwrap();
            if msg != *rep {
                return false;
            }
        }
        return rbuf.is_empty() && wbuf.is_empty();
    }

    #[test]
    fn request_reply() {
        let test = || {
            let process_num = 32usize;
            match prepare_fork(process_num) {
                ReqRep::Parent(pids, mut eps, req_reps) => {
                    for n in 0..process_num {
                        assert!(request_reply_server(&mut eps[n], &req_reps[n]));
                    }
                    for ep in eps.iter_mut() {
                        ep.close().unwrap();
                    }
                    for pid in pids {
                        let mut exit_status = unsafe {std::mem::uninitialized() };
                        if unsafe {
                            libc::waitpid(pid, &mut exit_status as *mut libc::c_int, 0 as libc::c_int)
                        } < 0 {
                            panic!("waitpid");
                        }
                        assert!(exit_status == 0);
                    }
                    (-1, 0)
                },
                ReqRep::Children(pid, mut ep, req_rep) => {
                    let mut exit_status = 0;
                    if !request_reply_client(&mut ep, &req_rep) {
                        exit_status = 1;
                    }
                    ep.close().unwrap();
                    (pid, exit_status)
                },
            }
        };
        let (pid, exit_status) = test();
        if pid > 0 {
            std::process::exit(exit_status);
        }
    }

    #[test]
    fn request_reply_poll() {
    }
}

struct PipeReadState {
    buf: Vec<u8>
}

struct PipeWriteState {
    buf: Vec<u8>
}

// information about a connection that is kept on the server side
struct ConnectionInfo {
    server_end_point: LinuxVfsIpcServerEndPoint,
    // NB: it is assumed clients's requests are unique (with respect to their canonical path), duplicated open for the same file should be
    // handled on the client side.
    opened_files: HashMap<PathBuf, Rc<MapInfo>>,
    read_state: PipeReadState,
    write_state: PipeWriteState,
}


// information about a established mmap,
// the ref-count is kept implicitly by Rc<MapInfo>
// the real_path is kept by the key of a HashMap<PathBuf, Rc<MapInfo>>
// NB: real_path should be canonical when appears in HashMap
struct MapInfo {
    // NB: make sure shm_name is null-terminated
    shm_name: String,
    length: libc::size_t,
}

impl MapInfo {
    // construct a mmap, currently you can not query vfs for the version of a file
    pub fn open(cont: &[u8], shm_name:String) -> LibcResult<Self> {
        let length = cont.len() as libc::size_t;
        unsafe {
            let shm_oflag = libc::O_CREAT | libc::O_EXCL | libc::O_RDWR;
            let shm_mode = libc::S_IRUSR | libc::S_IWUSR;
            let shm_fd = libc::shm_open(shm_name.as_ptr() as *const libc::c_char, shm_oflag, shm_mode);

            if shm_fd < 0 {
                handle_libc_error!("shm_open");
            }

            if libc::ftruncate(shm_fd, length as libc::off_t) < 0 {
                handle_libc_error!("ftruncate");
            }

            let mmap_prot = libc::PROT_READ | libc::PROT_WRITE;
            // shared map to save us a few memory pages
            // only the server write to the mapped area, the clients only read them, so no problem here
            let mmap_flags = libc::MAP_SHARED;
            let mmap_addr = libc::mmap(0 as *mut libc::c_void, length, mmap_prot, mmap_flags, shm_fd, 0);
            if mmap_addr == libc::MAP_FAILED {
                handle_libc_error!("mmap");
            }
            std::ptr::copy_nonoverlapping(cont.as_ptr() as *const u8, mmap_addr as *mut u8, length);
            if libc::munmap(mmap_addr, length as libc::size_t) < 0 {
                handle_libc_error!("munmap");
            }

            if libc::close(shm_fd) < 0 {
                handle_libc_error!("close");
            }
        }

        Ok(Self {
            shm_name,
            length,
        })
    }

    // close a shared memory, after closing, clients won't be able to "connect to" this mmap, but existing
    // shms are not invalidated.
    pub fn close(&self) -> LibcResult<()> {
        if unsafe {
            libc::shm_unlink(self.shm_name.as_ptr() as *const libc::c_char)
        } < 0 {
            handle_libc_error!("shm_unlink");
        }
        Ok(())
    }
}

// a server that takes care of handling client's requests and managin mmap
pub struct LinuxVfsIpcServer<U> {
    // need a Rc<RefCell<_>>, because we didn't want to consume the &mut self when taking a &mut
    // ConnectionInfo
    connection_infos: HashMap<Token, Rc<RefCell<ConnectionInfo>>>,
    // same reason as the Rc<RefCell<_>> for connection_infos
    live_maps: Rc<RefCell<HashMap<PathBuf, Weak<MapInfo>>>>,
    poll: Poll,
    vfs: Arc<Vfs<U>>,
    server_pid: u32,
    timestamp: usize
}

impl<U: Serialize + DeserializeOwned + Clone> LinuxVfsIpcServer<U> {
    fn handle_request(&mut self, tok: Token, ci: &mut ConnectionInfo, req: VfsRequestMsg) -> Result<()> {
        match req {
            VfsRequestMsg::OpenFile(path) => {
                self.handle_open_request(tok, ci, path)
            },
            VfsRequestMsg::CloseFile(path) => {
                self.handle_close_request(tok, ci, path)
            },
        }
    }

    fn setup_mmap(&mut self, path: &Path) -> Result<Rc<MapInfo>> {
        use super::super::FileContents;
        let shm_name = self.generate_shm_name(&path);
        match self.vfs.load_file(&path)? {
            FileContents::Text(s) => {
                Ok(Rc::new(MapInfo::open(s.as_bytes(), shm_name)?))
            }
            FileContents::Binary(v) => {
                Ok(Rc::new(MapInfo::open(&v, shm_name)?))
            }
        }
    }

    fn try_setup_mmap(&mut self, path: &Path) -> Result<(Rc<MapInfo>, U)> {
        // TODO: currently, vfs doesn't restrict which files are allowed to be opened, this may
        // need some change in the future.
        let path = path.canonicalize()?;

        // TODO: more efficient impl, less memory copy and lookup
        use std::collections::hash_map::Entry;
        let live_maps = self.live_maps.clone();
        let mut live_maps = live_maps.borrow_mut();
        let mi = match live_maps.entry(path.clone()) {
            Entry::Occupied(mut occ) => {
                match occ.get().upgrade() {
                    Some(rc) => {
                        rc
                    },
                    None => {
                        let mi = self.setup_mmap(&path)?;
                        occ.insert(std::rc::Rc::downgrade(&mi));
                        mi
                    }
                }
            },
            Entry::Vacant(vac) => {
                let mi = self.setup_mmap(&path)?;
                vac.insert(std::rc::Rc::downgrade(&mi));
                mi
            },
        };
        let u = self.vfs.with_user_data(&path, |res| {
            match res {
                Err(err) => Err(err),
                Ok((_, u)) => {
                    Ok(u.clone())
                },
            }
        })?;
        Ok((mi, u))
    }

    fn handle_open_request(&mut self, token: Token, ci: &mut ConnectionInfo, path: PathBuf) -> Result<()> {
        let (map_info, user_data) = self.try_setup_mmap(&path)?;
        let reply_msg = VfsReplyMsg::<U> {
            path: map_info.shm_name.clone(),
            length: map_info.length as u32,
            user_data
        };
        ci.opened_files.insert(path, map_info);
        self.write_reply(token, ci, reply_msg)
    }

    fn write_reply(&mut self, token: Token, ci: &mut ConnectionInfo, reply_msg: VfsReplyMsg<U>) -> Result<()> {
        // FIXME
        let old_len = ci.write_state.buf.len();
        {
            let mut ext = match bincode::serialize(&reply_msg) {
                Ok(ext) => ext,
                Err(err) => {
                    return Err(RlsVfsIpcError::SerializeError(err))
                }
            };
            ci.write_state.buf.append(&mut ext);
        }

        if old_len == 0usize {
            // this means the write-fd is not in the poll
            self.initial_write(token, ci)?;
        }
        Ok(())
        // else, there are on-going write on the event poll, which will carry this message
    }

    // the write-fd is not in the poll, first write as much as possible until EWOULDBLOCK, if still
    // some contents remain, register the write-fd to the poll
    fn initial_write(&mut self, token: Token, ci: &mut ConnectionInfo) -> Result<()> {
        let write_fd = ci.server_end_point.write_fd.get_fd()?;
        let len = ci.write_state.buf.len();
        let mut start_pos = 0usize;
        while start_pos < len {
            let res = 
            unsafe {
                libc::write(write_fd, &ci.write_state.buf[0] as *const u8 as *const libc::c_void, len - start_pos)
            };
            if res > 0 {
                start_pos += res as usize;
            } else if res == 0 {
                // same as EWOULDBLOCK
                break;
            } else {
                if would_block_or_error!("write") {
                    break;
                }
            }
        }
        ci.write_state.buf = ci.write_state.buf.split_off(start_pos);
        if start_pos < len {
            use mio::{event::Evented, unix::EventedFd};
            EventedFd(&write_fd).register(&self.poll, token, mio::Ready::writable(), mio::PollOpt::edge())?;
        }
        Ok(())
    }

    fn handle_close_request(&mut self, _tok: Token, ci: &mut ConnectionInfo, path: PathBuf) -> Result<()> {
        match ci.opened_files.remove(&path) {
            Some(mi) => {
                self.try_remove_last_map(mi, &path)?;
            }
            None => {
                return Err(RlsVfsIpcError::CloseNonOpenedFile);
            }
        }
        Ok(())
    }

    // a eof is met when reading a pipe, the connection's read side will not be used again(write
    // side may still be used to send replies)
    fn finish_read(&mut self, _tok: Token, ci: &mut ConnectionInfo) -> Result<()> {
        Ok(())
    }

    // try to read some requests and handle them
    fn handle_read(&mut self, token: Token, ci: &mut ConnectionInfo) -> Result<()> {
        // FIXME: this is ugly, but I don't want to spell a long name
        macro_rules! buf {
            () => {
                ci.read_state.buf
            }
        }

        let mut buf1:[u8;4096] = unsafe { std::mem::uninitialized() };
        let mut met_eof = false;
        let read_fd = ci.server_end_point.read_fd.get_fd()?;
        loop {
            let res = unsafe {
                libc::read(read_fd, &mut buf1[0] as *mut u8 as *mut libc::c_void, std::mem::size_of_val(&buf1))
            };
            if res > 0 {
                buf!().extend_from_slice(&buf1[..(res as usize)]);
            } else {
                match res {
                    0 => {
                        met_eof = true;
                        break;
                    },
                    _ => {
                        if would_block_or_error!("read") {
                            break;
                        }
                    }
                }
            }
        }

        let len = buf!().len();
        let mut start_pos = 0;
        while start_pos + 4 <= len {
            let msg_len = match bincode::deserialize::<u32>(&buf!()[start_pos..(start_pos + 4)]) {
                Ok(msg_len) => msg_len as usize,
                Err(err) => {
                    return Err(RlsVfsIpcError::DeserializeError(err));
                }
            };
            if msg_len + start_pos > len {
                break;
            }
            let msg:VfsRequestMsg = match bincode::deserialize(&buf!()[(start_pos+4)..(start_pos+msg_len)]) {
                Ok(msg) => msg,
                Err(err) => {
                    return Err(RlsVfsIpcError::DeserializeError(err));
                }
            };
            self.handle_request(token, ci, msg)?;
            start_pos += msg_len;
        }

        buf!() = buf!().split_off(start_pos);

        if met_eof {
            if buf!().is_empty() {
                self.finish_read(token, ci)?;
            } else {
                return Err(RlsVfsIpcError::PipeCloseMiddle);
            }
        }
        Ok(())
    }

    // try to write some replies to the pipe
    fn handle_write(&mut self, _token: Token, ci: &mut ConnectionInfo) -> Result<()> {
        macro_rules! buf {
            () => {
                ci.write_state.buf
            }
        };
        let len = buf!().len();
        let mut start_pos:usize = 0;
        let write_fd = ci.server_end_point.write_fd.get_fd()?;
        while len > start_pos {
            let res = unsafe {
                libc::write(write_fd, &buf!()[0] as *const u8 as *const libc::c_void, (len - start_pos) as libc::size_t)
            };
            if res > 0 {
                start_pos += res as usize;
            } else if res == 0 {
                // NB: same as EWOULDBLOCK
                break;
            } else {
                if would_block_or_error!("write") {
                    break;
                }
            }
        }

        buf!().split_off(start_pos);
        if buf!().is_empty() {
            use mio::{event::Evented, unix::EventedFd};
            EventedFd(&write_fd).deregister(&self.poll)?;
        }
        Ok(())
    }

    // make sure the generated name is null-terminated
    fn generate_shm_name(&self, file_path: &Path) -> String {
        let ret = std::format!("/rls-{}-{}-{}\u{0000}", self.server_pid, file_path.display(), self.timestamp);
        ret
    }

    fn try_remove_last_map(&mut self, mi: Rc<MapInfo>, file_path: &Path) -> Result<()> {
        if Rc::<MapInfo>::strong_count(&mi) == 1 {
            mi.close()?;
            self.live_maps.borrow_mut().remove(file_path);
        }
        Ok(())
    }
}

impl<U: Serialize + DeserializeOwned + Clone> VfsIpcServer<U> for LinuxVfsIpcServer<U> {
    type Channel = LinuxVfsIpcChannel;
    type ServerEndPoint = LinuxVfsIpcServerEndPoint;
    type ClientEndPoint = LinuxVfsIpcClientEndPoint;
    type Error = RlsVfsIpcError;

    fn new(vfs: Arc<Vfs<U>>) -> Result<Self> {
        Ok(Self {
            connection_infos: HashMap::new(),
            live_maps: Rc::new(RefCell::new(HashMap::new())),
            poll: Poll::new()?,
            vfs,
            server_pid: std::process::id(),
            timestamp: 0
        })
    }

    fn roll_the_loop(&mut self) -> Result<()> {
        // FIXME: a better capacity
        let mut events = mio::Events::with_capacity(64);
        loop {
            self.poll.poll(&mut events, None)?;
            for event in &events {
                let token = event.token();
                let ci = match self.connection_infos.get_mut(&token) {
                    Some(ci) => ci.clone(),
                    None => return Err(RlsVfsIpcError::TokenNotFound),
                };

                let ready = event.readiness();
                if ready.contains(mio::Ready::readable()) {
                    let ci = ci.clone();
                    self.handle_read(token, &mut ci.borrow_mut())?;
                }
                if ready.contains(mio::Ready::writable()) {
                    let ci = ci.clone();
                    self.handle_write(token, &mut ci.borrow_mut())?;
                }
            }
        }
    }

    fn add_server_end_point(&mut self, s_ep: Self::ServerEndPoint) -> Result<Token> {
        use mio::{event::Evented, unix::EventedFd};
        let read_fd = s_ep.read_fd.get_fd()?;
        // fd's are unique
        let tok_usize = read_fd as usize;
        let tok = Token(tok_usize);
        EventedFd(&read_fd).register(&self.poll, tok, mio::Ready::readable(), mio::PollOpt::edge())?;
        Ok(tok)
    }

    fn remove_server_end_point(&mut self, tok: Token) -> Result<()>{
        use mio::{event::Evented, unix::EventedFd};
        match self.connection_infos.remove(&tok) {
            Some(ci) => {
                let mut ci = ci.borrow_mut();
                let read_fd = ci.server_end_point.read_fd.get_fd()?;
                EventedFd(&read_fd).deregister(&self.poll)?;
                if ci.write_state.buf.len() != 0 {
                    let write_fd = ci.server_end_point.write_fd.get_fd()?;
                    EventedFd(&write_fd).deregister(&self.poll)?;
                }
                for (file_path, mi) in ci.opened_files.drain() {
                    self.try_remove_last_map(mi, &file_path)?;
                }
            },
            None => {
                return Err(RlsVfsIpcError::RemoveUnknownClient);
            }
        }
        Ok(())
    }
}

impl VfsIpcServerEndPoint for LinuxVfsIpcServerEndPoint {
    type Error = RlsVfsIpcError;
    type ReadBuffer = Vec<u8>;
    type WriteBuffer = Vec<u8>;

    fn blocking_read_request(&mut self, rbuf: &mut Self::ReadBuffer) -> Result<VfsRequestMsg> {
        blocking_read_impl::<VfsRequestMsg>(&self.read_fd, rbuf)
    }

    fn blocking_write_reply<U: Serialize + DeserializeOwned + Clone>(&mut self, rep: &VfsReplyMsg<U>, wbuf: &mut Self::WriteBuffer) -> Result<()> {
        blocking_write_impl(&self.write_fd, rep, wbuf)
    }
}

pub struct OpenedLinuxVfsIpcFileHandle {
    addr: *mut libc::c_void,
    length: libc::size_t,
}

impl OpenedLinuxVfsIpcFileHandle {
    pub fn close(&mut self) -> LibcResult<()> {
        if unsafe { libc::munmap(self.addr, self.length) } < 0 {
            handle_libc_error!("munmap");
        }
        Ok(())
    }
}

pub enum LinuxVfsIpcFileHandle {
    Open(OpenedLinuxVfsIpcFileHandle),
    Closed,
}

impl LinuxVfsIpcFileHandle {
    pub fn from_reply<U: Serialize + DeserializeOwned + Clone>(reply: VfsReplyMsg<U>) -> LibcResult<(Self, U)> {
        let addr;
        let length = reply.length as libc::size_t;
        unsafe {
            let shm_oflag = libc::O_RDONLY;
            let shm_mode: libc::mode_t = 0;
            let shm_fd = libc::shm_open(reply.path.as_ptr() as *const i8, shm_oflag, shm_mode);
            if shm_fd < 0 {
                handle_libc_error!("shm_open");
            }

            let mmap_prot = libc::PROT_READ;
            // shared map to save us a few memory pages
            // only the server write to the mapped area, the clients only read them, so no problem here
            let mmap_flags = libc::MAP_SHARED;
            addr = libc::mmap(0 as *mut libc::c_void, length, mmap_prot, mmap_flags, shm_fd, 0 as libc::off_t);
            if addr == libc::MAP_FAILED  {
                handle_libc_error!("mmap");
            }

            if libc::close(shm_fd) < 0 {
                handle_libc_error!("close");
            }
        }

        Ok((Self::Open(OpenedLinuxVfsIpcFileHandle {
            addr,
            length,
        }), reply.user_data))
    }

    pub fn close(&mut self) -> LibcResult<()> {
        match self {
            Self::Open(handle) => {
                handle.close()?;
                *self = Self::Closed;
                Ok(())
            },
            Self::Closed => {
                fake_libc_error!("LinuxVfsIpcFileHandle::close" ,libc::EBADF);
            },
        }
    }
}

impl VfsIpcFileHandle for LinuxVfsIpcFileHandle {
    type Error = RlsVfsIpcError;
    fn get_file_ref(&self) -> Result<&str> {
        // NB: whether the file contents are valid utf8 are never checked
        match self {
            Self::Open(handle) => {
                Ok(unsafe {
                    let slice = std::slice::from_raw_parts(handle.addr as *const u8, handle.length as usize);
                    std::str::from_utf8_unchecked(&slice)
                })
            },
            Self::Closed => {
                return Err(RlsVfsIpcError::GetFileFromClosedHandle);
            }
        }
    }
}

impl Drop for LinuxVfsIpcFileHandle {
    fn drop(&mut self) {
        match self {
            Self::Open(_) => {
                panic!("you drop a LinuxVfsIpcFileHanlde while it's still open")
            },
            Self::Closed => (),
        }
    }
}

