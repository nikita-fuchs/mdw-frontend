#![feature(plugin)]
#![feature(slice_concat_ext)]
#![feature(custom_attribute)]
#![feature(proc_macro_hygiene, decl_macro)]
#![feature(try_trait)]
extern crate backtrace;
extern crate base58;
extern crate base58check;
extern crate bigdecimal;
extern crate blake2;
extern crate blake2b;
extern crate byteorder;
extern crate chashmap;
extern crate chrono;
extern crate clap;
extern crate crypto;
extern crate curl;
extern crate daemonize;
#[macro_use]
extern crate diesel;
#[macro_use]
extern crate diesel_migrations;
extern crate dotenv;
extern crate env_logger;
extern crate flexi_logger;
extern crate futures;
extern crate hex;
extern crate itertools;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate log;
extern crate r2d2;
extern crate r2d2_diesel;
extern crate r2d2_postgres;
extern crate rand;
extern crate regex;
extern crate reqwest;
#[macro_use]
extern crate rocket;
#[macro_use]
extern crate rocket_contrib;
extern crate rocket_cors;
extern crate rust_base58;
extern crate rust_decimal;
#[macro_use]
extern crate serde_derive;
extern crate postgres;
extern crate serde_json;
extern crate ws;

use std::thread;
use std::thread::JoinHandle;

use clap::{App, Arg};

use std::env;

pub mod coinbase;
pub mod hashing;
pub mod loader;
pub mod middleware_result;
pub mod node;
pub mod schema;
pub mod server;
pub mod websocket;

pub use bigdecimal::BigDecimal;
use loader::BlockLoader;
use middleware_result::MiddlewareResult;
use server::MiddlewareServer;
pub mod models;

use daemonize::Daemonize;
use diesel::PgConnection;
use dotenv::dotenv;
use r2d2::Pool;
use r2d2_diesel::ConnectionManager;
use r2d2_postgres::PostgresConnectionManager;
use std::sync::Arc;

const VERSION: &'static str = env!("CARGO_PKG_VERSION");

embed_migrations!("migrations/");

lazy_static! {
    static ref PGCONNECTION: Arc<Pool<ConnectionManager<PgConnection>>> = {
        dotenv().ok(); // Grabbing ENV vars
        let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let manager = ConnectionManager::<PgConnection>::new(database_url);
        let pool = r2d2::Pool::builder()
            .max_size(20) // only used for emergencies...
            .build(manager)
            .expect("Failed to create pool.");
        Arc::new(pool)
    };
}

lazy_static! {
    static ref SQLCONNECTION: Arc<Pool<PostgresConnectionManager>> = {
        dotenv().ok(); // Grabbing ENV vars
        let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let manager = PostgresConnectionManager::new
            (database_url, r2d2_postgres::TlsMode::None).unwrap();
        let pool = r2d2::Pool::builder()
            .max_size(3) // only used for emergencies...
            .build(manager)
            .expect("Failed to create pool.");
        Arc::new(pool)
    };
}

#[derive(PartialEq)]
enum ParanoiaLevel {
    Normal,
    High,
}

lazy_static! {
    static ref PARANOIA_LEVEL: ParanoiaLevel = {
        let paranoia_level = env::var("PARANOIA_LEVEL");
        match paranoia_level {
            Ok(x) => {
                if x.eq(&String::from("high")) {
                    ParanoiaLevel::High
                } else {
                    ParanoiaLevel::Normal
                }
            }
            _ => ParanoiaLevel::Normal,
        }
    };
}

/*
 * This function does two things--initially it asks the DB for the
* heights not present between 0 and the height returned by
* /generations/current.  After it has queued all of them it spawns the
* detect_forks thread, then it starts the blockloader, which does not
* return.
*/

fn fill_missing_heights(url: String, _tx: std::sync::mpsc::Sender<i64>) -> MiddlewareResult<bool> {
    debug!("In fill_missing_heights()");
    let node = node::Node::new(url.clone());
    let top_block = node::key_block_from_json(node.latest_key_block().unwrap()).unwrap();
    let missing_heights = node.get_missing_heights(top_block.height)?;
    for height in missing_heights {
        debug!("Adding {} to load queue", &height);
        match loader::queue(height as i64, &_tx) {
            Ok(_) => (),
            Err(x) => {
                error!("Error queuing block to send: {}", x);
                BlockLoader::recover_from_db_error();
            }
        };
    }
    _tx.send(loader::BACKLOG_CLEARED)?;
    Ok(true)
}

fn main() {
    match env::var("LOG_DIR") {
        Ok(x) => {
            flexi_logger::Logger::with_env()
                .log_to_file()
                .directory(x)
                .start()
                .unwrap();
            ()
        }
        Err(_x) => env_logger::Builder::from_default_env()
            .target(env_logger::Target::Stdout)
            .init(),
    }
    let matches = App::new("æternity middleware")
        .version(VERSION)
        .author("John Newby <john@newby.org>")
        .about("----")
        .arg(
            Arg::with_name("server")
                .short("s")
                .long("server")
                .help("Start server")
                .takes_value(false),
        )
        .arg(
            Arg::with_name("populate")
                .short("p")
                .long("populate")
                .help("Populate DB")
                .takes_value(false),
        )
        .arg(
            Arg::with_name("daemonize")
                .short("-d")
                .long("daemonize")
                .help("Daemonize process")
                .takes_value(false),
            )
        .arg(
            Arg::with_name("verify")
                .short("v")
                .long("verify")
                .help("Verify DB integrity against chain")
                .takes_value(false),
        )
        .arg(
            Arg::with_name("heights")
                .short("H")
                .long("heights")
                .help("Load specific heights, values separated by comma, ranges with from-to accepted")
                .takes_value(true),
            )
        .arg(
            Arg::with_name("websocket")
                .short("w")
                .long("websocket")
                .help("Activate websocket (only valid when -p (populate) option also set")
                .requires("populate")
                .takes_value(false),
            )
        .get_matches();

    let url = env::var("NODE_URL")
        .expect("NODE_URL must be set")
        .to_string();

    let populate = matches.is_present("populate");
    let serve = matches.is_present("server");
    let verify = matches.is_present("verify");
    let heights = matches.is_present("heights");
    let daemonize = matches.is_present("daemonize");
    let websocket = matches.is_present("websocket");

    if daemonize {
        let daemonize = Daemonize::new();
        if let Ok(x) = env::var("PID_FILE") {
            daemonize.pid_file(x).start();
        } else {
            daemonize.start();
        }
    }

    if verify {
        debug!("Verifying");
        let loader = BlockLoader::new(url.clone());
        match loader.verify() {
            Ok(_) => (),
            Err(x) => error!("Blockloader::verify() returned an error: {}", x),
        };
        return;
    }

    // Run migrations iff populate set
    if populate {
        let connection = PGCONNECTION.get().unwrap();
        let mut migration_output = Vec::new();
        let migration_result =
            embedded_migrations::run_with_output(&*connection, &mut migration_output);
        for line in migration_output.iter() {
            info!("migration out: {}", line);
        }
        migration_result.unwrap();
    }

    /*
     * The `heights` argument is of this form: 1,10-15,1000 which
     * would cause blocks 1, 10,11,12,13,14,15 and 1000 to be loaded.
     */
    if heights {
        let to_load = matches.value_of("heights").unwrap();
        let loader = BlockLoader::new(url.clone());
        for h in to_load.split(',') {
            let s = String::from(h);
            match s.find("-") {
                Some(_) => {
                    let fromto: Vec<String> = s.split('-').map(|x| String::from(x)).collect();
                    for i in fromto[0].parse::<i64>().unwrap()..fromto[1].parse::<i64>().unwrap() {
                        loader.load_blocks(i).unwrap();
                    }
                }
                None => {
                    loader.load_blocks(s.parse::<i64>().unwrap()).unwrap();
                }
            }
        }
    }

    let mut populate_thread: Option<JoinHandle<()>> = None;

    /*
     * We start 3 populate processes--one queries for missing heights
     * and works through that list, then exits. Another polls for
     * new blocks to load, then sleeps and does it again, and yet
     * another reads the mempool (if available).
     */
    if populate {
        let url = url.clone();
        let loader = BlockLoader::new(url.clone());
        match fill_missing_heights(url.clone(), loader.tx.clone()) {
            Ok(_) => (),
            Err(x) => error!("fill_missing_heights() returned an error: {}", x),
        };
        populate_thread = Some(thread::spawn(move || {
            loader.start();
        }));
        if websocket {
            websocket::start_ws();
        }
    }

    if serve {
        let ms: MiddlewareServer = MiddlewareServer {
            node: node::Node::new(url.clone()),
            dest_url: url.to_string(),
            port: 3013,
        };
        ms.start();
        loop {
            // just to stop main() thread exiting.
            thread::sleep(std::time::Duration::new(40, 0));
        }
    }
    if !populate && !serve && !heights {
        warn!("Nothing to do!");
    }

    /*
     * If we have a populate thread running, wait for it to exit.
     */
    match populate_thread {
        Some(x) => {
            x.join();
            ()
        }
        None => (),
    }
}
