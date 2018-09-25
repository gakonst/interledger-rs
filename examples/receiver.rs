extern crate ilp;
extern crate tokio;
extern crate bytes;
extern crate futures;
extern crate ring;
extern crate chrono;
extern crate env_logger;

use tokio::prelude::*;
use ilp::plugin::btp::connect_to_moneyd;
use ilp::stream::Connection;
use ilp::spsp::listen_with_random_secret;
use futures::{Stream};

fn main() {
  env_logger::init();

  let future = connect_to_moneyd()
  .and_then(move |plugin| {
    println!("Conected receiver");

    listen_with_random_secret(plugin, 3000)
      .and_then(|listener| {
        listener.for_each(|conn: Connection| {
          println!("Got incoming connection");
          let handle_connection = conn.for_each(|stream| {
            println!("Got incoming stream");
            stream.for_each(|amount| {
              println!("Got incoming money {}", amount);
              Ok(())
            })
          });

          tokio::spawn(handle_connection);
          Ok(())
        })
        .map_err(|err| {
          println!("Error in listener {:?}", err);
        })
        .map(|_| ())
      })
  });

  tokio::runtime::run(future);
}