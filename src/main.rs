#![doc(html_root_url = "https://docs.rs/quote-miner/0.0.1")]
#![warn(clippy::pedantic)]
#![allow(clippy::filter_map)]
#![allow(clippy::map_unwrap_or)]
//TODO:
#![allow(dead_code)]

use dot::{GraphWalk, Id, Labeller, Style};
use egg_mode::{
	tweet::{self, Tweet},
	user::TwitterUser,
	KeyPair, RateLimit, Response, Token,
};
use maplit::hashset;
use std::{
	convert::TryInto as _,
	io::{self, stdin},
	ops::Deref,
	sync::atomic::{AtomicBool, Ordering},
	time::{Duration, SystemTime, UNIX_EPOCH},
};
use structopt::StructOpt;
use tokio::{fs::OpenOptions, io::AsyncWriteExt};
use try_match::try_match;
use wyz::Pipe as _;

#[cfg(doctest)]
pub mod readme {
	doc_comment::doctest!("../README.md");
}

#[derive(Debug)]
struct Config {
	pub consumer: KeyPair,
	pub token: Token,
}

impl Config {
	pub fn load() -> Self {
		let consumer = KeyPair {
			key: include_str!("../secrets/consumer_key").trim().into(),
			secret: include_str!("../secrets/consumer_secret_key").trim().into(),
		};
		Self {
			consumer: consumer.clone(),
			token: Token::Access {
				// https://developer.twitter.com/en/apps
				consumer,
				access: KeyPair {
					key: include_str!("../secrets/access").trim().into(),
					secret: include_str!("../secrets/access_secret").trim().into(),
				},
			},
		}
	}
}

#[derive(Debug, StructOpt)]
#[structopt(name = "quote-miner", about = "Makes fancy graphs")]
struct Opt {
	pub ids: Vec<u64>,
	#[structopt(subcommand)]
	pub cmd: Option<Command>,
}

#[derive(Debug, StructOpt)]
enum Command {
	Login,
}

static STOP: AtomicBool = AtomicBool::new(false);

async fn sleep_until(unix: i32) {
	while let Some(duration) = (UNIX_EPOCH + Duration::from_secs(unix.try_into().unwrap()))
		.duration_since(SystemTime::now())
		.ok()
	{
		eprintln!("Sleeping for {:?}...", duration);
		let duration = (duration + Duration::from_secs(5)).min(Duration::from_secs(10));
		tokio::time::sleep(duration).await;

		if STOP.load(Ordering::SeqCst) {
			break;
		}
	}
}

async fn limit<T>(response: Response<T>) -> T {
	let RateLimit {
		limit,
		remaining,
		reset,
	} = response.rate_limit_status;

	let remaining_time = (UNIX_EPOCH + Duration::from_secs(reset.try_into().unwrap()))
		.duration_since(SystemTime::now())
		.ok();

	eprintln!(
		"{remaining}/{limit} remaining for {reset}",
		remaining = remaining,
		limit = limit,
		reset = remaining_time
			.as_ref()
			.map(|remaining| format!("{:?}", remaining))
			.unwrap_or_else(|| "now".into())
	);

	if remaining == 0 {
		sleep_until(reset).await
	}

	response.response
}

#[tokio::main()]
async fn main() {
	ctrlc::set_handler(move || {
		eprintln!("Stopping.");
		STOP.store(true, Ordering::SeqCst);
	})
	.ok();

	let config = Config::load();
	let token = &config.token;

	let opt = Opt::from_args();
	if let Some(Command::Login) = opt.cmd {
		let token = egg_mode::auth::request_token(&config.consumer, "oob")
			.await
			.unwrap();
		let url = egg_mode::auth::authorize_url(&token);
		println!("{}", url);
		let mut pin = String::new();
		stdin().read_line(&mut pin).unwrap();

		let (token, _user_id, username) =
			egg_mode::auth::access_token(config.consumer, &token, pin)
				.await
				.unwrap();

		match token {
			Token::Access {
				consumer: _,
				access,
			} => {
				let options = OpenOptions::new().truncate(true).write(true).to_owned();

				options
					.open("secrets/access")
					.await
					.unwrap()
					.write_all(access.key.as_bytes())
					.await
					.unwrap();
				options
					.open("secrets/access_secret")
					.await
					.unwrap()
					.write_all(access.secret.as_bytes())
					.await
					.unwrap();
			}
			Token::Bearer(_) => todo!(),
		}

		println!("Logged in as @{}", username)
	}

	let mut queue = opt.ids.clone();
	let mut visited = hashset![];

	let mut found = vec![];

	while !queue.is_empty() {
		if STOP.load(Ordering::SeqCst) {
			eprintln!("Stopping early.");
			break;
		}

		let tweets: Vec<_> = queue
			.drain(..queue.len().min(10))
			.filter(|id| visited.insert(*id))
			.collect();

		let batch_size = tweets.len();

		let tweets = tweet::lookup(tweets, token).await.unwrap();
		let tweets = limit(tweets).await;

		let missing = batch_size - tweets.len();
		if missing > 0 {
			eprintln!("Got {} tweets less than expected.", missing)
		}

		for tweet in tweets {
			println!(
				r#"https://twitter.com/{}/status/{} => {}: "{}""#,
				tweet.user.as_ref().unwrap().screen_name,
				tweet.id,
				tweet.user.as_ref().unwrap().name,
				tweet.text
			);

			if let Some(quoted) = tweet.quoted_status_id {
				queue.push(quoted)
			}

			//Quoting Tweets
			let quoting = loop {
				if STOP.load(Ordering::SeqCst) {
					break vec![];
				}
				match egg_mode::search::search(format!(
					r"https://twitter.com/{}/status/{}",
					tweet.user.as_ref().unwrap().screen_name,
					tweet.id,
				))
				.call(token)
				.await
				{
					Ok(quoting) => break limit(quoting).await.statuses,
					Err(egg_mode::error::Error::RateLimit(reset)) => {
						eprintln!("Search rate limit exceeded.");
						sleep_until(reset).await;
						continue;
					}
					Err(err) => Err(err).unwrap(),
				}
			};
			for quoting in quoting {
				queue.push(quoting.id)
			}

			found.push(tweet);
		}
		eprintln!("{} found, {} remaining.", found.len(), queue.len());
	}

	let output = OpenOptions::new()
		.create(true)
		.write(true)
		.truncate(true)
		.open(format!(
			"{} {}.dot",
			opt.ids
				.iter()
				.map(ToString::to_string)
				.collect::<Vec<_>>()
				.as_slice()
				.join(" "),
			SystemTime::now()
				.duration_since(SystemTime::UNIX_EPOCH)
				.unwrap()
				.as_secs()
		))
		.await
		.unwrap();

	tokio::task::spawn_blocking(move || {
		struct W<AW>(AW);

		impl<AW: AsyncWriteExt + Unpin> io::Write for W<AW> {
			fn write(&mut self, data: &[u8]) -> io::Result<usize> {
				tokio::runtime::Runtime::new()
					.unwrap()
					.block_on(self.0.write(data))
			}
			fn flush(&mut self) -> io::Result<()> {
				todo!()
			}
		}

		dot::render(
			&Output(
				found
					.iter()
					.map(Node::Tweet)
					//.chain(found.iter().map(|t| Node::User(t.user.as_ref().unwrap())))
					.collect::<Vec<_>>()
					.as_slice(),
				found
					.iter()
					.filter(|t| found.iter().any(|qrtd| Some(qrtd.id) == t.quoted_status_id))
					.map(Edge::Qrt)
					//.chain(found.iter().map(Edge::Tw))
					.collect::<Vec<_>>()
					.as_slice(),
			),
			&mut W(output),
		)
		.unwrap();
	})
	.await
	.unwrap()
}

struct Output<'a>(&'a [Node<'a>], &'a [Edge<'a>]);

#[derive(Debug, Copy, Clone)]
enum Node<'a> {
	Tweet(&'a Tweet),
	User(&'a TwitterUser),
}

#[derive(Debug, Copy, Clone)]
enum Edge<'a> {
	Qrt(&'a Tweet),
	Tw(&'a Tweet),
}

impl<'a> Labeller<'a, Node<'a>, Edge<'a>> for Output<'a> {
	fn graph_id(&'a self) -> Id<'a> {
		Id::new("reply_chain").unwrap()
	}

	fn node_id(&'a self, n: &Node) -> Id<'a> {
		match n {
			Node::Tweet(n) => dot::Id::new(format!("N{}", n.id)).unwrap(),
			Node::User(n) => dot::Id::new(format!("U{}", n.id)).unwrap(),
		}
	}

	fn node_shape(&'a self, _node: &Node) -> Option<dot::LabelText<'a>> {
		None
	}

	fn node_label(&'a self, n: &Node) -> dot::LabelText<'a> {
		match n {
			Node::Tweet(n) => dot::LabelText::EscStr(
				format!(
					"{} @{}\n\n{}",
					n.user.as_ref().unwrap().name,
					n.user.as_ref().unwrap().screen_name,
					n.text
				)
				.into(),
			),
			Node::User(n) => {
				dot::LabelText::EscStr(format!("{} @{}", n.name, n.screen_name).into())
			}
		}
	}

	fn edge_label(&'a self, e: &Edge) -> dot::LabelText<'a> {
		let _ignored = e;
		dot::LabelText::LabelStr("".into())
	}

	fn node_style(&'a self, _n: &Node) -> Style {
		Style::None
	}

	fn node_color(&'a self, _node: &Node) -> Option<dot::LabelText<'a>> {
		None
	}

	fn edge_end_arrow(&'a self, _e: &Edge) -> dot::Arrow {
		dot::Arrow::default()
	}

	fn edge_start_arrow(&'a self, _e: &Edge) -> dot::Arrow {
		dot::Arrow::default()
	}

	fn edge_style(&'a self, _e: &Edge) -> Style {
		Style::None
	}

	fn edge_color(&'a self, _e: &Edge) -> Option<dot::LabelText<'a>> {
		None
	}

	fn kind(&self) -> dot::Kind {
		dot::Kind::Digraph
	}
}

impl<'a> GraphWalk<'a, Node<'a>, Edge<'a>> for Output<'a> {
	fn nodes(&'a self) -> dot::Nodes<'a, Node> {
		self.0.into()
	}

	fn edges(&'a self) -> dot::Edges<'a, Edge<'a>> {
		self.1.into()
	}

	fn source(&'a self, edge: &Edge) -> Node<'_> {
		match edge {
			Edge::Qrt(e) => self
				.0
				.iter()
				.filter_map(|n| try_match!(Node::Tweet(a) = n => a).ok())
				.find(|n| n.id == e.id)
				.unwrap()
				.deref()
				.pipe(Node::Tweet),
			Edge::Tw(e) => self
				.0
				.iter()
				.filter_map(|n| try_match!(Node::User(a) = n => a).ok())
				.find(|n| n.id == e.user.as_ref().unwrap().id)
				.unwrap()
				.deref()
				.pipe(Node::User),
		}
	}

	fn target(&'a self, edge: &Edge) -> Node<'a> {
		match edge {
			Edge::Qrt(e) => self
				.0
				.iter()
				.filter_map(|n| try_match!(Node::Tweet(a) = n => a).ok())
				.find(|n| n.id == e.quoted_status_id.unwrap())
				.unwrap()
				.deref()
				.pipe(Node::Tweet),
			Edge::Tw(e) => self
				.0
				.iter()
				.filter_map(|n| try_match!(Node::Tweet(a) = n => a).ok())
				.find(|n| n.id == e.id)
				.unwrap()
				.deref()
				.pipe(Node::Tweet),
		}
	}
}
