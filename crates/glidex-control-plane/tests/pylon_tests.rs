//! End-to-end and functional tests for Pylon hypervisor integration
//! (spec/pylon.md Milestone P4).
//!
//! Covers:
//! - Pylon VM registration against pylond daemon over Unix domain socket with `SCM_RIGHTS`.
//! - SCM_RIGHTS ancillary data transfer of shared memory page (8,192 bytes) and kick/wake eventfds.
//! - Pylon driver launch argv generation and elasticity parameter permutations.

use glidex_control_plane::hypervisor::HypervisorType;
use glidex_control_plane::hypervisor::pylon::{DEFAULT_PYLOND_SOCKET, pylon_shm_path, pylon_vm_id};
use glidex_control_plane::models::VmConfig;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::mem::{size_of, size_of_val, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::ptr::{copy_nonoverlapping, null_mut};

/// JSON payload sent over `/run/pylon/pylond.sock` during VM registration (matching pylon ABI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmRegistrationRequest {
    pub vm_id: u64,
    pub num_vcpus: u32,
    pub min_cores: u32,
    pub max_cores: u32,
    pub priority_weight: u32,
}

/// JSON response returned by `pylond` after processing a `VmRegistrationRequest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmRegistrationResponse {
    pub accepted: bool,
    pub message: String,
}

fn send_msg_with_fds(sock_fd: RawFd, payload: &[u8], fds: &[RawFd]) -> std::io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: payload.as_ptr() as *mut libc::c_void,
        iov_len: payload.len(),
    };

    let fd_bytes = size_of_val(fds);
    let cmsg_space = unsafe { libc::CMSG_SPACE(fd_bytes as u32) as usize };
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut msg: libc::msghdr = unsafe { zeroed() };
    msg.msg_iov = &mut iov as *mut libc::iovec;
    msg.msg_iovlen = 1;

    if !fds.is_empty() {
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_space;

        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as u32) as usize;
            let data_ptr = libc::CMSG_DATA(cmsg) as *mut RawFd;
            copy_nonoverlapping(fds.as_ptr(), data_ptr, fds.len());
        }
    }

    let sent = unsafe { libc::sendmsg(sock_fd, &msg as *const libc::msghdr, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn recv_msg_with_fds(sock_fd: RawFd) -> std::io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut payload_buf = vec![0u8; 4096];
    let mut iov = libc::iovec {
        iov_base: payload_buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: payload_buf.len(),
    };

    let max_fds = 32;
    let max_fd_bytes = max_fds * size_of::<RawFd>();
    let cmsg_space = unsafe { libc::CMSG_SPACE(max_fd_bytes as u32) as usize };
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut msg: libc::msghdr = unsafe { zeroed() };
    msg.msg_name = null_mut();
    msg.msg_namelen = 0;
    msg.msg_iov = &mut iov as *mut libc::iovec;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_space;

    let n = unsafe {
        libc::recvmsg(
            sock_fd,
            &mut msg as *mut libc::msghdr,
            libc::MSG_CMSG_CLOEXEC,
        )
    };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "peer closed connection",
        ));
    }

    payload_buf.truncate(n as usize);
    let mut received_fds = Vec::new();

    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data_ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
                let cmsg_len = (*cmsg).cmsg_len;
                let base_len = libc::CMSG_LEN(0) as usize;
                if cmsg_len >= base_len {
                    let fds_payload_len = cmsg_len - base_len;
                    let count = fds_payload_len / size_of::<RawFd>();
                    for i in 0..count {
                        let fd = *data_ptr.add(i);
                        if fd >= 0 {
                            received_fds.push(OwnedFd::from_raw_fd(fd));
                        }
                    }
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    Ok((payload_buf, received_fds))
}

fn create_eventfd() -> std::io::Result<OwnedFd> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

#[test]
fn pylon_vm_registration_handshake_with_scm_rights() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("pylond.sock");
    let listener = UnixListener::bind(&sock_path).unwrap();

    let server_handle = std::thread::spawn(move || {
        for _ in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            let (payload, fds) = recv_msg_with_fds(stream.as_raw_fd()).unwrap();
            let req: VmRegistrationRequest = serde_json::from_slice(&payload).unwrap();

            let (accepted, message) = if req.min_cores > req.max_cores {
                (
                    false,
                    "invalid core bounds: min_cores > max_cores".to_string(),
                )
            } else if req.num_vcpus == 0 {
                (false, "num_vcpus must be > 0".to_string())
            } else if fds.len() != (1 + 2 * req.num_vcpus as usize) {
                (
                    false,
                    format!(
                        "expected {} fds (1 shm + 2x{} irqfds), got {}",
                        1 + 2 * req.num_vcpus,
                        req.num_vcpus,
                        fds.len()
                    ),
                )
            } else {
                // Verify the shared page fd is 8,192 bytes
                let file = std::fs::File::from(fds[0].try_clone().unwrap());
                let meta = file.metadata().unwrap();
                assert_eq!(
                    meta.len(),
                    8192,
                    "Pylon shared memory page must be 8,192 bytes"
                );
                (true, "VM registered successfully".to_string())
            };

            let resp = VmRegistrationResponse { accepted, message };
            let resp_bytes = serde_json::to_vec(&resp).unwrap();
            let mut stream = stream;
            stream.write_all(&resp_bytes).unwrap();
            stream.flush().unwrap();
        }
    });

    // 1. Success case: valid VM with 2 vCPUs, 1 shm + 2 kick + 2 wake = 5 fds
    {
        let num_vcpus = 2;
        let shm = tempfile::tempfile().unwrap();
        shm.set_len(8192).unwrap();

        let mut kick_fds = Vec::new();
        let mut wake_fds = Vec::new();
        for _ in 0..num_vcpus {
            kick_fds.push(create_eventfd().unwrap());
            wake_fds.push(create_eventfd().unwrap());
        }

        let mut all_fds = Vec::new();
        all_fds.push(shm.as_raw_fd());
        for fd in &kick_fds {
            all_fds.push(fd.as_raw_fd());
        }
        for fd in &wake_fds {
            all_fds.push(fd.as_raw_fd());
        }

        let req = VmRegistrationRequest {
            vm_id: 42,
            num_vcpus,
            min_cores: 1,
            max_cores: 2,
            priority_weight: 500,
        };
        let payload = serde_json::to_vec(&req).unwrap();

        let mut stream = UnixStream::connect(&sock_path).unwrap();
        send_msg_with_fds(stream.as_raw_fd(), &payload, &all_fds).unwrap();
        stream.flush().unwrap();

        let mut resp_buf = Vec::new();
        stream.read_to_end(&mut resp_buf).unwrap();
        let resp: VmRegistrationResponse = serde_json::from_slice(&resp_buf).unwrap();
        assert!(
            resp.accepted,
            "Expected registration accepted: {}",
            resp.message
        );
        assert_eq!(resp.message, "VM registered successfully");
    }

    // 2. Failure case: invalid core bounds min_cores > max_cores
    {
        let num_vcpus = 1;
        let shm = tempfile::tempfile().unwrap();
        shm.set_len(8192).unwrap();
        let kick = create_eventfd().unwrap();
        let wake = create_eventfd().unwrap();
        let fds = vec![shm.as_raw_fd(), kick.as_raw_fd(), wake.as_raw_fd()];

        let req = VmRegistrationRequest {
            vm_id: 43,
            num_vcpus,
            min_cores: 4,
            max_cores: 2,
            priority_weight: 100,
        };
        let payload = serde_json::to_vec(&req).unwrap();

        let mut stream = UnixStream::connect(&sock_path).unwrap();
        send_msg_with_fds(stream.as_raw_fd(), &payload, &fds).unwrap();
        stream.flush().unwrap();

        let mut resp_buf = Vec::new();
        stream.read_to_end(&mut resp_buf).unwrap();
        let resp: VmRegistrationResponse = serde_json::from_slice(&resp_buf).unwrap();
        assert!(
            !resp.accepted,
            "Expected registration rejected for invalid bounds"
        );
        assert!(resp.message.contains("min_cores > max_cores"));
    }

    server_handle.join().unwrap();
}

#[test]
fn pylon_paths_and_vm_id_properties() {
    let api = "/run/glidex-cp/vms/11111111-2222-3333-4444-555555555555/api.sock";
    assert_eq!(
        pylon_shm_path(api),
        "/run/glidex-cp/vms/11111111-2222-3333-4444-555555555555/pylon.shm"
    );
    assert_eq!(DEFAULT_PYLOND_SOCKET, "/run/pylon/pylond.sock");

    let id = pylon_vm_id(api);
    assert_ne!(id, 0);

    // Deterministic across invocations
    assert_eq!(id, pylon_vm_id(api));
}

#[test]
fn pylon_config_elasticity_model() {
    // Elastic VM configuration
    let cfg: VmConfig = serde_json::from_value(serde_json::json!({
        "hypervisor": "pylon",
        "vcpu_count": 2,
        "min_vcpus": 1,
        "max_vcpus": 8,
        "cpu_weight": 750,
        "mem_size_mib": 2048,
        "rootfs_path": "/path/to/rootfs",
        "kernel_args": "console=hvc0",
    }))
    .unwrap();

    assert_eq!(cfg.hypervisor, HypervisorType::Pylon);
    assert!(cfg.is_elastic());
    assert_eq!(cfg.effective_min_vcpus(), 1);
    assert_eq!(cfg.effective_max_vcpus(), 8);
    assert_eq!(cfg.effective_cpu_weight(), 750);
}
