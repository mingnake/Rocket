use std::io;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::task::{Poll, Context};

use tokio::io::ReadBuf;

/// ```text
/// [                    capacity                     ]
/// [       initialized        |     uninitialized    ]
/// [ +++ filled +++ | -------- unfilled              ]
///       ^------ cursor
/// ```
///
/// ```text
/// [ ++++*+++++++++++---------xxxxxxxxxxxxxxxxxxxxxxx]
/// ```
///
/// * `+`: filled (implies initialized)
/// * `*`: cursor position (implies filled)
/// * `-`: unfilled (implies initialized)
/// * `x`: uninitialized (implies unfilled)
pub struct TransformBuf<'a, 'b> {
    pub(crate) buf: &'a mut ReadBuf<'b>,
    pub(crate) cursor: usize,
}

pub trait Transform {
    fn poll_transform(
        self: Pin<&mut Self>,
        buf: &mut TransformBuf<'_, '_>,
    ) -> io::Result<()>;

    fn poll_finish(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut TransformBuf<'_, '_>,
    ) -> Poll<io::Result<()>> {
        let (_, _) = (cx, buf);
        Poll::Ready(Ok(()))
    }
}

impl TransformBuf<'_, '_> {
    pub fn fresh(&self) -> &[u8] {
        &self.filled()[self.cursor..]
    }

    pub fn fresh_mut(&mut self) -> &mut [u8] {
        let cursor = self.cursor;
        &mut self.filled_mut()[cursor..]
    }

    pub fn spoil(&mut self) {
        let cursor = self.cursor;
        self.set_filled(cursor);
    }
}

pub struct Inspect(pub(crate) Box<dyn FnMut(&[u8]) + Send + Sync + 'static>);

impl Transform for Inspect {
    fn poll_transform(
        mut self: Pin<&mut Self>,
        // _: &mut Context<'_>,
        buf: &mut TransformBuf<'_, '_>,
    ) -> io::Result<()> {
        (self.0)(buf.fresh());
        Ok(())
    }
}

pub struct InPlaceMap(
    Box<dyn FnMut(&mut TransformBuf<'_, '_>) -> io::Result<()> + Send + Sync + 'static>
);

impl Transform for InPlaceMap {
    fn poll_transform(
        mut self: Pin<&mut Self>,
        buf: &mut TransformBuf<'_, '_>,
    ) -> io::Result<()> {
        (self.0)(buf)
    }
}

impl crate::Data<'_> {
    /// Chain an inspect [`Transform`] to `self`.
    pub fn chain_inspect<F: FnMut(&[u8])>(&mut self, f: F) -> &mut Self
        where F: Send + Sync + 'static
    {
        self.chain_transform(Inspect(Box::new(f)))
    }

    /// Chain an infallible in-place map [`Transform`] to `self`.
    pub fn chain_inplace_map<F: FnMut(&mut TransformBuf<'_, '_>)>(&mut self, mut f: F) -> &mut Self
        where F: Send + Sync + 'static
    {
        self.chain_transform(InPlaceMap(Box::new(move |buf| Ok(f(buf)))))
    }

    /// Chain a fallible in-place map [`Transform`] to `self`.
    pub fn chain_try_inplace_map<F>(&mut self, f: F) -> &mut Self
        where F: FnMut(&mut TransformBuf<'_, '_>) -> io::Result<()> + Send + Sync + 'static
    {
        self.chain_transform(InPlaceMap(Box::new(f)))
    }

    /// Chain an in-place hash [`Transform`] to `self`.
    pub fn chain_hash_transform<H: std::hash::Hasher>(&mut self, hasher: H) -> &mut Self
        where H: Unpin + Send + Sync + 'static
    {
        self.chain_transform(hash_transform::HashTransform { hasher, hash: None })
    }
}

mod hash_transform {
    use std::io::Cursor;
    use std::hash::Hasher;

    use tokio::io::AsyncRead;

    use super::*;

    pub struct HashTransform<H: Hasher> {
        pub(crate) hasher: H,
        pub(crate) hash: Option<Cursor<[u8; 8]>>
    }

    impl<H: Hasher + Unpin> Transform for HashTransform<H> {
        fn poll_transform(
            mut self: Pin<&mut Self>,
            // cx: &mut Context<'_>,
            buf: &mut TransformBuf<'_, '_>,
        ) -> io::Result<()> {
            self.hasher.write(buf.fresh());
            buf.spoil();
            Ok(())
        }

        fn poll_finish(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut TransformBuf<'_, '_>,
        ) -> Poll<io::Result<()>> {
            if self.hash.is_none() {
                let hash = self.hasher.finish();
                self.hash = Some(Cursor::new(hash.to_be_bytes()));
            }

            let cursor = self.hash.as_mut().unwrap();
            Pin::new(cursor).poll_read(cx, buf)
        }
    }
}

impl<'a, 'b> Deref for TransformBuf<'a, 'b> {
    type Target = ReadBuf<'b>;

    fn deref(&self) -> &Self::Target {
        &self.buf
    }
}

impl<'a, 'b> DerefMut for TransformBuf<'a, 'b> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buf
    }
}

// TODO: Test chaining various transform combinations:
//  * consume | consume
//  * add | consume
//  * consume | add
//  * add | add
// Where `add` is a transformer that adds data to the stream, and `consume` is
// one that removes data.
#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use std::hash::SipHasher;
    use std::sync::{Arc, atomic::{AtomicU64, AtomicU8}};

    use parking_lot::Mutex;
    use ubyte::ToByteUnit;

    use crate::http::Method;
    use crate::local::blocking::Client;
    use crate::fairing::AdHoc;
    use crate::{route, Route, Data, Response, Request};

    #[test]
    fn test_transform_series() {
        fn handler<'r>(_: &'r Request<'_>, data: Data<'r>) -> route::BoxFuture<'r> {
            Box::pin(async move {
                data.open(128.bytes()).stream_to(tokio::io::sink()).await.expect("read ok");
                route::Outcome::Success(Response::new())
            })
        }

        let inspect2: Arc<AtomicU8> = Arc::new(AtomicU8::new(0));
        let raw_data: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let hash: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
        let rocket = crate::build()
            .manage(hash.clone())
            .manage(raw_data.clone())
            .manage(inspect2.clone())
            .mount("/", vec![Route::new(Method::Post, "/", handler)])
            .attach(AdHoc::on_request("transforms", |req, data| Box::pin(async {
                let hash1 = req.rocket().state::<Arc<AtomicU64>>().cloned().unwrap();
                let hash2 = req.rocket().state::<Arc<AtomicU64>>().cloned().unwrap();
                let raw_data = req.rocket().state::<Arc<Mutex<Vec<u8>>>>().cloned().unwrap();
                let inspect2 = req.rocket().state::<Arc<AtomicU8>>().cloned().unwrap();
                data.chain_inspect(move |bytes| { *raw_data.lock() = bytes.to_vec(); })
                    .chain_hash_transform(SipHasher::new())
                    .chain_inspect(move |bytes| {
                        assert_eq!(bytes.len(), 8);
                        let bytes: [u8; 8] = bytes.try_into().expect("[u8; 8]");
                        let value = u64::from_be_bytes(bytes);
                        hash1.store(value, atomic::Ordering::Release);
                    })
                    .chain_inspect(move |bytes| {
                        assert_eq!(bytes.len(), 8);
                        let bytes: [u8; 8] = bytes.try_into().expect("[u8; 8]");
                        let value = u64::from_be_bytes(bytes);
                        let prev = hash2.load(atomic::Ordering::Acquire);
                        assert_eq!(prev, value);
                        inspect2.fetch_add(1, atomic::Ordering::Release);
                    });
            })));

        // Make sure nothing has happened yet.
        assert!(raw_data.lock().is_empty());
        assert_eq!(hash.load(atomic::Ordering::Acquire), 0);
        assert_eq!(inspect2.load(atomic::Ordering::Acquire), 0);

        // Check that nothing happens if the data isn't read.
        let client = Client::untracked(rocket).unwrap();
        client.get("/").body("Hello, world!").dispatch();
        assert!(raw_data.lock().is_empty());
        assert_eq!(hash.load(atomic::Ordering::Acquire), 0);
        assert_eq!(inspect2.load(atomic::Ordering::Acquire), 0);

        // Check inspect + hash + inspect + inspect.
        client.post("/").body("Hello, world!").dispatch();
        assert_eq!(raw_data.lock().as_slice(), "Hello, world!".as_bytes());
        assert_eq!(hash.load(atomic::Ordering::Acquire), 0xae5020d7cf49d14f);
        assert_eq!(inspect2.load(atomic::Ordering::Acquire), 1);

        // Check inspect + hash + inspect + inspect, round 2.
        let string = "Rocket, Rocket, where art thee? Oh, tis in the sky, I see!";
        client.post("/").body(string).dispatch();
        assert_eq!(raw_data.lock().as_slice(), string.as_bytes());
        assert_eq!(hash.load(atomic::Ordering::Acquire), 0x323f9aa98f907faf);
        assert_eq!(inspect2.load(atomic::Ordering::Acquire), 2);
    }
}
