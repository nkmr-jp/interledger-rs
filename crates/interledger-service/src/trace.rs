use crate::*;
use async_trait::async_trait;
use tracing_futures::{Instrument, Instrumented};

// TODO see if we can replace this with the tower tracing later
#[async_trait]
impl<IO, A> IncomingService<A> for Instrumented<IO>
where
    IO: IncomingService<A> + Clone + Send,
    A: Account + 'static,
{
    async fn handle_request(&mut self, request: IncomingRequest<A>) -> IlpResult {
        println!("[MY_LOG INSPECT] IncomingService.handle_request() request.prepare.destination: {:?} {}:{} ",request.prepare.destination(), file!(), line!());
        self.inner_mut()
            .handle_request(request)
            .in_current_span()
            .await
    }
}

#[async_trait]
impl<IO, A> OutgoingService<A> for Instrumented<IO>
where
    IO: OutgoingService<A> + Clone + Send,
    A: Account + 'static,
{
    async fn send_request(&mut self, request: OutgoingRequest<A>) -> IlpResult {
        println!("[MY_LOG INSPECT] OutgoingService.send_request() request.prepare.destination: {:?} {}:{} ",request.prepare.destination(), file!(), line!());
        self.inner_mut()
            .send_request(request)
            .in_current_span()
            .await
    }
}
