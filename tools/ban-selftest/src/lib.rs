//! One planted use of every disallowed method and type in the root `clippy.toml`.
//!
//! `check.py` reads the trailing `ban-method:` and `ban-type:` markers, runs clippy with the root
//! configuration and fails unless every marked line is reported. Keep each use and its marker on one line.
//! Nothing here is ever called.
#![allow(unused)]

use std::time::Duration;

pub fn std_instant_now() { let _ = std::time::Instant::now(); } // ban-method: std::time::Instant::now
pub fn std_system_time_now() { let _ = std::time::SystemTime::now(); } // ban-method: std::time::SystemTime::now
pub fn tokio_instant_now() { let _ = tokio::time::Instant::now(); } // ban-method: tokio::time::Instant::now
pub async fn tokio_sleep() { tokio::time::sleep(Duration::ZERO).await; } // ban-method: tokio::time::sleep
pub async fn tokio_sleep_until(t: tokio::time::Instant) { tokio::time::sleep_until(t).await; } // ban-method: tokio::time::sleep_until
pub async fn tokio_timeout() { let _ = tokio::time::timeout(Duration::ZERO, async {}).await; } // ban-method: tokio::time::timeout
pub fn tokio_interval() { let _ = tokio::time::interval(Duration::from_secs(1)); } // ban-method: tokio::time::interval
pub fn std_thread_sleep() { std::thread::sleep(Duration::ZERO); } // ban-method: std::thread::sleep
pub fn std_thread_spawn() { let _ = std::thread::spawn(|| {}); } // ban-method: std::thread::spawn
pub fn tokio_spawn() { let _ = tokio::spawn(async {}); } // ban-method: tokio::spawn
pub fn tokio_task_spawn() { let _ = tokio::task::spawn(async {}); } // ban-method: tokio::task::spawn
pub fn tokio_spawn_blocking() { let _ = tokio::task::spawn_blocking(|| {}); } // ban-method: tokio::task::spawn_blocking
pub fn tokio_spawn_local() { let _ = tokio::task::spawn_local(async {}); } // ban-method: tokio::task::spawn_local
pub fn tokio_block_in_place() { tokio::task::block_in_place(|| {}); } // ban-method: tokio::task::block_in_place
pub fn rand_rng() { let _ = rand::rng(); } // ban-method: rand::rng
pub fn rand_random() { let _: u8 = rand::random(); } // ban-method: rand::random
pub fn rand_thread_rng() { let _ = rand08::thread_rng(); } // ban-method: rand::thread_rng
pub fn getrandom_fill(buf: &mut [u8]) { let _ = getrandom::fill(buf); } // ban-method: getrandom::fill
pub fn getrandom_getrandom(buf: &mut [u8]) { let _ = getrandom02::getrandom(buf); } // ban-method: getrandom::getrandom
pub fn uuid_new_v4() { let _ = uuid::Uuid::new_v4(); } // ban-method: uuid::Uuid::new_v4
pub fn uuid_now_v7() { let _ = uuid::Uuid::now_v7(); } // ban-method: uuid::Uuid::now_v7
pub fn env_var() { let _ = std::env::var("X"); } // ban-method: std::env::var
pub fn env_var_os() { let _ = std::env::var_os("X"); } // ban-method: std::env::var_os
pub fn env_vars() { let _ = std::env::vars(); } // ban-method: std::env::vars
pub fn fs_read() { let _ = std::fs::read("x"); } // ban-method: std::fs::read
pub fn fs_read_to_string() { let _ = std::fs::read_to_string("x"); } // ban-method: std::fs::read_to_string
pub fn fs_write() { let _ = std::fs::write("x", b""); } // ban-method: std::fs::write
pub fn fs_create_dir_all() { let _ = std::fs::create_dir_all("x"); } // ban-method: std::fs::create_dir_all
pub fn fs_remove_file() { let _ = std::fs::remove_file("x"); } // ban-method: std::fs::remove_file
pub fn fs_rename() { let _ = std::fs::rename("x", "y"); } // ban-method: std::fs::rename
pub fn fs_read_dir() { let _ = std::fs::read_dir("x"); } // ban-method: std::fs::read_dir
pub fn fs_metadata() { let _ = std::fs::metadata("x"); } // ban-method: std::fs::metadata
pub async fn tokio_fs_read() { let _ = tokio::fs::read("x").await; } // ban-method: tokio::fs::read
pub async fn tokio_fs_write() { let _ = tokio::fs::write("x", b"").await; } // ban-method: tokio::fs::write
pub fn std_tcp_connect() { let _ = std::net::TcpStream::connect("127.0.0.1:1"); } // ban-method: std::net::TcpStream::connect
pub fn std_tcp_bind() { let _ = std::net::TcpListener::bind("127.0.0.1:0"); } // ban-method: std::net::TcpListener::bind
pub fn std_udp_bind() { let _ = std::net::UdpSocket::bind("127.0.0.1:0"); } // ban-method: std::net::UdpSocket::bind
pub async fn tokio_tcp_connect() { let _ = tokio::net::TcpStream::connect("127.0.0.1:1").await; } // ban-method: tokio::net::TcpStream::connect
pub async fn tokio_tcp_bind() { let _ = tokio::net::TcpListener::bind("127.0.0.1:0").await; } // ban-method: tokio::net::TcpListener::bind
pub fn std_command_new() { let _ = std::process::Command::new("x"); } // ban-method: std::process::Command::new
pub fn tokio_command_new() { let _ = tokio::process::Command::new("x"); } // ban-method: tokio::process::Command::new
pub fn hashbrown_map_new() { let _ = hashbrown::HashMap::<u8, u8>::new(); } // ban-method: hashbrown::HashMap::new
pub fn hashbrown_map_with_capacity() { let _ = hashbrown::HashMap::<u8, u8>::with_capacity(1); } // ban-method: hashbrown::HashMap::with_capacity
pub fn hashbrown_set_new() { let _ = hashbrown::HashSet::<u8>::new(); } // ban-method: hashbrown::HashSet::new
pub fn hashbrown_set_with_capacity() { let _ = hashbrown::HashSet::<u8>::with_capacity(1); } // ban-method: hashbrown::HashSet::with_capacity
pub fn scope_mint(id: crucible_records::ProjectId) { let _ = crucible_records::ProjectScope::mint(id); } // ban-method: crucible_records::ProjectScope::mint

pub fn std_hash_map(_: &std::collections::HashMap<u8, u8>) {} // ban-type: std::collections::HashMap
pub fn std_hash_set(_: &std::collections::HashSet<u8>) {} // ban-type: std::collections::HashSet
pub fn std_fs_file(_: &std::fs::File) {} // ban-type: std::fs::File
pub fn std_fs_open_options(_: &std::fs::OpenOptions) {} // ban-type: std::fs::OpenOptions
pub fn tokio_fs_file(_: &tokio::fs::File) {} // ban-type: tokio::fs::File
pub fn tokio_fs_open_options(_: &tokio::fs::OpenOptions) {} // ban-type: tokio::fs::OpenOptions
