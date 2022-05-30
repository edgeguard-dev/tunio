use super::Interface;
use crate::config::Layer;
use crate::traits::AsyncQueueT;
use crate::traits::QueueT;
use crate::Error;
use delegate::delegate;
use libc::{IFF_NO_PI, IFF_TAP, IFF_TUN};
use netconfig::sys::posix::ifreq::ifreq;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{fs, io};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

mod ioctls {
    nix::ioctl_write_int!(tunsetiff, b'T', 202);
    nix::ioctl_write_int!(tunsetpersist, b'T', 203);
    nix::ioctl_write_int!(tunsetowner, b'T', 204);
    nix::ioctl_write_int!(tunsetgroup, b'T', 206);
}

pub(crate) struct Device {
    pub device: fs::File,
    pub name: String,
}

pub(crate) fn create_device(name: &str, layer: Layer) -> Result<Device, Error> {
    let tun_device = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/net/tun")?;

    let mut init_flags = match layer {
        Layer::L2 => IFF_TAP,
        Layer::L3 => IFF_TUN,
    };
    init_flags |= IFF_NO_PI;

    let mut req = ifreq::new(name);
    req.ifr_ifru.ifru_flags = init_flags as _;

    unsafe { ioctls::tunsetiff(tun_device.as_raw_fd(), &req as *const _ as _) }.unwrap();

    // Name can change due to formatting
    Ok(Device {
        device: tun_device,
        name: String::try_from(&req.ifr_ifrn)
            .map_err(|e| Error::InterfaceNameError(format!("{e:?}")))?,
    })
}

// AsyncTokioQueue
