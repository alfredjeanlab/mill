use std::future::{Future, IntoFuture};
use std::marker::PhantomData;
use std::pin::Pin;
use std::time::Duration;

pub fn poll<'a, F: Fn() -> bool + 'a>(condition: F) -> WaitFor<'a, F> {
    WaitFor { condition, timeout: Duration::from_secs(1), msg: "condition not met" }
}

pub struct WaitFor<'a, F> {
    condition: F,
    timeout: Duration,
    msg: &'a str,
}

impl<'a, F: Fn() -> bool + 'a> WaitFor<'a, F> {
    pub fn secs(mut self, n: u64) -> Self {
        self.timeout = Duration::from_secs(n);
        self
    }

    pub fn expect(mut self, msg: &'a str) -> Self {
        self.msg = msg;
        self
    }
}

impl<'a, F: Fn() -> bool + 'a> IntoFuture for WaitFor<'a, F> {
    type Output = ();
    type IntoFuture = Pin<Box<dyn Future<Output = ()> + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let deadline = tokio::time::Instant::now() + self.timeout;
            while !(self.condition)() {
                assert!(tokio::time::Instant::now() < deadline, "timeout: {}", self.msg);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
    }
}

pub fn poll_async<'a, F, Fut>(condition: F) -> WaitForAsync<'a, F, Fut>
where
    F: FnMut() -> Fut + 'a,
    Fut: Future<Output = bool> + 'a,
{
    WaitForAsync {
        condition,
        timeout: Duration::from_secs(1),
        msg: "condition not met",
        _fut: PhantomData,
    }
}

pub struct WaitForAsync<'a, F, Fut> {
    condition: F,
    timeout: Duration,
    msg: &'a str,
    _fut: PhantomData<fn() -> Fut>,
}

impl<'a, F, Fut> WaitForAsync<'a, F, Fut>
where
    F: FnMut() -> Fut + 'a,
    Fut: Future<Output = bool> + 'a,
{
    pub fn secs(mut self, n: u64) -> Self {
        self.timeout = Duration::from_secs(n);
        self
    }

    pub fn expect(mut self, msg: &'a str) -> Self {
        self.msg = msg;
        self
    }
}

impl<'a, F, Fut> IntoFuture for WaitForAsync<'a, F, Fut>
where
    F: FnMut() -> Fut + 'a,
    Fut: Future<Output = bool> + 'a,
{
    type Output = ();
    type IntoFuture = Pin<Box<dyn Future<Output = ()> + 'a>>;

    fn into_future(mut self) -> Self::IntoFuture {
        Box::pin(async move {
            let deadline = tokio::time::Instant::now() + self.timeout;
            while !(self.condition)().await {
                assert!(tokio::time::Instant::now() < deadline, "timeout: {}", self.msg);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
    }
}
