use std::cmp::{min};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::io::{Read, Seek, SeekFrom, Result};

use byteorder::{BigEndian, ReadBytesExt};
use encoding_rs::SHIFT_JIS;
use log::{debug};

use super::{action_state, buttons, character, frame, game, stage, triggers, ubjson};
use super::action_state::{Common, State};
use super::attack::{Attack};
use super::character::{Internal};
use super::frame::{Pre, Post, Direction, Position};
use super::game::{Start, End, Player, PlayerType, NUM_PORTS};

const ZELDA_TRANSFORM_FRAME:u32 = 43;
const SHEIK_TRANSFORM_FRAME:u32 = 36;

// We only track this for Sheik/Zelda transformations, which can't happen on
// the first frame. So we can initialize with any arbitrary character value.
const DEFAULT_CHAR_STATE:CharState = CharState {
	character: Internal(255),
	state: State::Common(Common::WAIT),
	age: 0
};

#[derive(Copy, Clone, Debug, PartialEq)]
struct CharState {
	character: Internal,
	state: State,
	age: u32,
}

#[derive(Debug, PartialEq, num_enum::TryFromPrimitive)]
#[repr(u8)]
pub enum Event {
	Payloads = 0x35,
	GameStart = 0x36,
	FramePre = 0x37,
	FramePost = 0x38,
	GameEnd = 0x39,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct FrameId {
	pub index: i32,
	pub port: u8,
	pub is_follower: bool,
}

#[derive(Debug)]
pub struct FrameEvent<F> {
	pub id: FrameId,
	pub event: F,
}

fn event_payloads<R:Read>(r:&mut R) -> Result<HashMap<u8, u16>> {
	let code = r.read_u8()?;
	if code != Event::Payloads as u8 {
		Err(err!("expected event payloads, but got: {}", code))?;
	}

	let payload_size = r.read_u8()?; // includes size byte for some reason
	if payload_size % 3 != 1 {
		Err(err!("invalid payload size: {}", payload_size))?;
	}

	let mut payload_sizes = HashMap::new();
	for _ in (0..(payload_size - 1)).step_by(3) {
		payload_sizes.insert(r.read_u8()?, r.read_u16::<BigEndian>()?);
	}

	Ok(payload_sizes)
}

fn player_v1_3(r:[u8; 16]) -> Result<game::PlayerV1_3> {
	let first_null = r.iter().position(|&x| x == 0).unwrap_or(16);
	let (name_tag, _) = SHIFT_JIS.decode_without_bom_handling(&r[0..first_null]);
	Ok(game::PlayerV1_3 {
		name_tag: name_tag.to_string(),
	})
}

fn player_v1_0(r:[u8; 8], v1_3:Option<[u8; 16]>) -> Result<game::PlayerV1_0> {
	let mut r = &r[..];
	Ok(game::PlayerV1_0 {
		ucf: game::Ucf {
			dash_back: match r.read_u32::<BigEndian>()? {
				0 => None,
				db => Some(game::DashBack(db)),
			},
			shield_drop: match r.read_u32::<BigEndian>()? {
				0 => None,
				sd => Some(game::ShieldDrop(sd)),
			},
		},
		#[cfg(v1_3)] v1_3: player_v1_3(v1_3.unwrap())?,
		#[cfg(not(v1_3))] v1_3: match v1_3 {
			Some(v1_3) => Some(player_v1_3(v1_3)?),
			None => None,
		},
	})
}

fn player(v0:&[u8; 36], is_teams:bool, v1_0:Option<[u8; 8]>, v1_3:Option<[u8; 16]>) -> Result<Option<Player>> {
	let mut r = &v0[..];
	let character = character::External(r.read_u8()?);
	let r#type = game::PlayerType(r.read_u8()?);
	let stocks = r.read_u8()?;
	let costume = r.read_u8()?;
	r.read_exact(&mut [0; 3])?; // ???
	let team_shade = r.read_u8()?;
	let handicap = r.read_u8()?;
	let team_color = r.read_u8()?;
	let team = {
		match is_teams {
			true => Some(game::Team {
				color: game::TeamColor(team_color),
				shade: game::TeamShade(team_shade),
			}),
			false => None,
		}
	};
	r.read_u16::<BigEndian>()?; // ???
	let bitfield = r.read_u8()?;
	r.read_u16::<BigEndian>()?; // ???
	let cpu_level = {
		let cpu_level = r.read_u8()?;
		match r#type {
			PlayerType::CPU => Some(cpu_level),
			_ => None,
		}
	};
	r.read_u32::<BigEndian>()?; // ???
	let offense_ratio = r.read_f32::<BigEndian>()?;
	let defense_ratio = r.read_f32::<BigEndian>()?;
	let model_scale = r.read_f32::<BigEndian>()?;
	r.read_u32::<BigEndian>()?; // ???
	// total bytes: 0x24

	#[cfg(v1_0)] let v1_0 = player_v1_0(v1_0.unwrap(), v1_3)?;
	#[cfg(not(v1_0))] let v1_0 = match v1_0 {
		Some(v1_0) => Some(player_v1_0(v1_0, v1_3)?),
		None => None,
	};

	Ok(match r#type {
		PlayerType::HUMAN | PlayerType::CPU | PlayerType::DEMO => Some(Player {
			character: character,
			r#type: r#type,
			stocks: stocks,
			costume: costume,
			team: team,
			handicap: handicap,
			bitfield: bitfield,
			cpu_level: cpu_level,
			offense_ratio: offense_ratio,
			defense_ratio: defense_ratio,
			model_scale: model_scale,
			v1_0: v1_0,
		}),
		_ => None
	})
}

fn player_bytes_v1_3(r:&mut &[u8]) -> Result<[u8; 16]> {
	let mut buf = [0; 16];
	r.read_exact(&mut buf)?;
	Ok(buf)
}

fn player_bytes_v1_0(r:&mut &[u8]) -> Result<[u8; 8]> {
	let mut buf = [0; 8];
	r.read_exact(&mut buf)?;
	Ok(buf)
}

fn game_start_v2_0(r:&mut &[u8]) -> Result<game::StartV2_0> {
	Ok(game::StartV2_0 {
		is_frozen_ps: r.read_u8()? != 0,
	})
}

fn game_start_v1_5(r:&mut &[u8]) -> Result<game::StartV1_5> {
	Ok(game::StartV1_5 {
		is_pal: r.read_u8()? != 0,
		#[cfg(v2_0)] v2_0: game_start_v2_0()?,
		#[cfg(not(v2_0))] v2_0: match r.is_empty() {
			true => None,
			_ => Some(game_start_v2_0(r)?),
		},
	})
}

fn game_start(mut r:&mut &[u8]) -> Result<Start> {
	debug!("game::Start");

	let slippi = game::Slippi {
		version: game::SlippiVersion(r.read_u8()?, r.read_u8()?, r.read_u8()?),
	};

	r.read_u8()?; // unused (build number)
	let bitfield = {
		let mut buf = [0; 3];
		buf[0] = r.read_u8()?; // bitfield 1
		buf[1] = r.read_u8()?; // bitfield 2
		r.read_u8()?; // ???
		buf[2] = r.read_u8()?; // bitfield 3
		buf
	};
	r.read_u32::<BigEndian>()?; // ???
	let is_teams = r.read_u8()? != 0;
	r.read_u16::<BigEndian>()?; // ???
	let item_spawn_frequency = r.read_i8()?;
	let self_destruct_score = r.read_i8()?;
	r.read_u8()?; // ???
	let stage = stage::Stage(r.read_u16::<BigEndian>()?);
	let timer = r.read_u32::<BigEndian>()?;
	r.read_exact(&mut [0; 15])?; // ???
	let item_spawn_bitfield = {
		let mut buf = [0; 5];
		r.read_exact(&mut buf)?;
		buf
	};
	r.read_u64::<BigEndian>()?; // ???
	let damage_ratio = r.read_f32::<BigEndian>()?;
	r.read_exact(&mut [0; 44])?; // ???
	// @0x65
	let mut players_v0 = [[0; 36]; 4];
	for p in &mut players_v0 {
		r.read_exact(p)?;
	}
	// @0xf5
	r.read_exact(&mut [0; 72])?; // ???
	// @0x13d
	let random_seed = r.read_u32::<BigEndian>()?;

	let players_v1_0 = match !cfg!(v1_0) && r.is_empty() {
		true => [None, None, None, None],
		_ => [Some(player_bytes_v1_0(&mut r)?), Some(player_bytes_v1_0(&mut r)?), Some(player_bytes_v1_0(&mut r)?), Some(player_bytes_v1_0(&mut r)?)],
	};

	let players_v1_3 = match !cfg!(v1_3) && r.is_empty() {
		true => [None, None, None, None],
		_ => [Some(player_bytes_v1_3(&mut r)?), Some(player_bytes_v1_3(&mut r)?), Some(player_bytes_v1_3(&mut r)?), Some(player_bytes_v1_3(&mut r)?)],
	};

	let players = [
		player(&players_v0[0], is_teams, players_v1_0[0], players_v1_3[0])?,
		player(&players_v0[1], is_teams, players_v1_0[1], players_v1_3[1])?,
		player(&players_v0[2], is_teams, players_v1_0[2], players_v1_3[2])?,
		player(&players_v0[3], is_teams, players_v1_0[3], players_v1_3[3])?,
	];

	#[cfg(v1_5)] let v1_5 = game_start_v1_5(r)?;
	#[cfg(not(v1_5))] let v1_5 = match r.is_empty() {
		true => None,
		_ => Some(game_start_v1_5(r)?),
	};

	Ok(Start {
		slippi: slippi,
		bitfield: bitfield,
		is_teams: is_teams,
		item_spawn_frequency: item_spawn_frequency,
		self_destruct_score: self_destruct_score,
		stage: stage,
		timer: timer,
		item_spawn_bitfield: item_spawn_bitfield,
		damage_ratio: damage_ratio,
		players: players,
		random_seed: random_seed,
		v1_5: v1_5,
	})
}

fn game_end_v2_0(r:&mut &[u8]) -> Result<game::EndV2_0> {
	Ok(game::EndV2_0 {
		lras_initiator: r.read_i8()?,
	})
}

fn game_end(r:&mut &[u8]) -> Result<End> {
	debug!("game::End");
	Ok(End {
		method: game::EndMethod(r.read_u8()?),
		#[cfg(v2_0)] v2_0: game_end_v2_0(r)?,
		#[cfg(not(v2_0))] v2_0: match r.is_empty() {
			true => None,
			_ => Some(game_end_v2_0(r)?),
		},
	})
}

fn direction(value:f32) -> Result<Direction> {
	match value {
		v if v < 0.0 => Ok(Direction::LEFT),
		v if v > 0.0 => Ok(Direction::RIGHT),
		_ => Err(err!("direction == 0")),
	}
}

fn predict_character(id:FrameId, last_char_states:&[CharState; NUM_PORTS]) -> Internal {
	let prev = last_char_states[id.port as usize];
	match prev.state {
		State::Zelda(action_state::Zelda::TRANSFORM_GROUND) |
		State::Zelda(action_state::Zelda::TRANSFORM_AIR)
			if prev.age >= ZELDA_TRANSFORM_FRAME => Internal::SHEIK,
		State::Sheik(action_state::Sheik::TRANSFORM_GROUND) |
		State::Sheik(action_state::Sheik::TRANSFORM_AIR)
			if prev.age >= SHEIK_TRANSFORM_FRAME => Internal::ZELDA,
		_ => prev.character,
	}
}

fn frame_pre_v1_4(r:&mut &[u8]) -> Result<frame::PreV1_4> {
	Ok(frame::PreV1_4 {
		damage: r.read_f32::<BigEndian>()?,
	})
}

fn frame_pre_v1_2(r:&mut &[u8]) -> Result<frame::PreV1_2> {
	Ok(frame::PreV1_2 {
		raw_analog_x: r.read_u8()?,
		#[cfg(v1_4)] v1_4: frame_pre_v1_4(r)?,
		#[cfg(not(v1_4))] v1_4: match r.is_empty() {
			true => None,
			_ => Some(frame_pre_v1_4(r)?),
		},
	})
}

fn frame_pre(r:&mut &[u8], last_char_states:&[CharState; NUM_PORTS]) -> Result<FrameEvent<Pre>> {
	let id = FrameId {
		index: r.read_i32::<BigEndian>()?,
		port: r.read_u8()?,
		is_follower: r.read_u8()? != 0,
	};
	debug!("frame::Pre: {:?}", id);

	// We need to know the character to interpret the action state properly, but for Sheik/Zelda we
	// don't know whether they transformed this frame untilwe get the corresponding frame::Post
	// event. So we predict based on whether we were on the last frame of `TRANSFORM_AIR` or
	// `TRANSFORM_GROUND` during the *previous* frame.
	let character = predict_character(id, last_char_states);

	let random_seed = r.read_u32::<BigEndian>()?;
	let state = State::from(r.read_u16::<BigEndian>()?, character);

	let position = Position {
		x: r.read_f32::<BigEndian>()?,
		y: r.read_f32::<BigEndian>()?,
	};
	let direction = direction(r.read_f32::<BigEndian>()?)?;
	let joystick = Position {
		x: r.read_f32::<BigEndian>()?,
		y: r.read_f32::<BigEndian>()?,
	};
	let cstick = Position {
		x: r.read_f32::<BigEndian>()?,
		y: r.read_f32::<BigEndian>()?,
	};
	let trigger_logical = r.read_f32::<BigEndian>()?;
	let buttons = frame::Buttons {
		logical: buttons::Logical(r.read_u32::<BigEndian>()?),
		physical: buttons::Physical(r.read_u16::<BigEndian>()?),
	};
	let triggers = frame::Triggers {
		logical: trigger_logical,
		physical: triggers::Physical {
			l: r.read_f32::<BigEndian>()?,
			r: r.read_f32::<BigEndian>()?,
		},
	};

	#[cfg(v1_2)] let v1_2 = frame_pre_v1_2(r)?;
	#[cfg(not(v1_2))] let v1_2 = match r.is_empty() {
		true => None,
		_ => Some(frame_pre_v1_2(r)?),
	};

	Ok(FrameEvent {
		id: id,
		event: Pre {
			index: id.index,
			random_seed: random_seed,
			state: state,
			position: position,
			direction: direction,
			joystick: joystick,
			cstick: cstick,
			triggers: triggers,
			buttons: buttons,
			v1_2: v1_2,
		}
	})
}

fn flags(buf:&[u8; 5]) -> frame::StateFlags {
	frame::StateFlags(
		((buf[0] as u64) << 00) +
		((buf[1] as u64) << 08) +
		((buf[2] as u64) << 16) +
		((buf[3] as u64) << 24) +
		((buf[4] as u64) << 32)
	)
}

fn update_last_char_state(id:FrameId, character:Internal, state:State, last_char_states:&mut [CharState; NUM_PORTS]) {
	let prev = last_char_states[id.port as usize];

	last_char_states[id.port as usize] = CharState {
		character: character,
		state: state,
		age: match state {
			s if s == prev.state => prev.age + 1,
			// `TRANSFORM_GROUND` and TRANSFORM_AIR can transition into each other without
			// interrupting the transformation, so treat them the same for age purposes
			State::Zelda(action_state::Zelda::TRANSFORM_GROUND) =>
				match prev.state {
					State::Zelda(action_state::Zelda::TRANSFORM_AIR) =>
						// If you land on the frame where you would have transitioned from
						// `TRANSFORM_AIR` to `TRANSFORM_AIR_ENDING`, you instead transition to
						// `TRANSFORM_GROUND` for one frame before going to
						// `TRANSFORM_GROUND_ENDING` on the next frame. This delays the character
						// switch by one frame, so we cap `age` at its previous value so as not to
						// confuse `predict_character`.
						min(ZELDA_TRANSFORM_FRAME - 1, prev.age + 1),
					_ => 0,
				},
			State::Zelda(action_state::Zelda::TRANSFORM_AIR) =>
				match prev.state {
					State::Zelda(action_state::Zelda::TRANSFORM_GROUND) =>
						min(ZELDA_TRANSFORM_FRAME - 1, prev.age + 1),
					_ => 0,
				},
			State::Sheik(action_state::Sheik::TRANSFORM_GROUND) =>
				match prev.state {
					State::Sheik(action_state::Sheik::TRANSFORM_AIR) =>
						min(SHEIK_TRANSFORM_FRAME - 1, prev.age + 1),
					_ => 0,
				},
			State::Sheik(action_state::Sheik::TRANSFORM_AIR) =>
				match prev.state {
					State::Sheik(action_state::Sheik::TRANSFORM_GROUND) =>
						min(SHEIK_TRANSFORM_FRAME - 1, prev.age + 1),
					_ => 0,
				},
			_ => 0,
		},
	};
}

fn frame_post_v2_1(r:&mut &[u8]) -> Result<frame::PostV2_1> {
	Ok(frame::PostV2_1 {
		hurtbox_state: frame::HurtboxState(r.read_u8()?),
	})
}

fn frame_post_v2_0(r:&mut &[u8]) -> Result<frame::PostV2_0> {
	Ok(frame::PostV2_0 {
		flags: {
			let mut buf = [0; 5];
			r.read_exact(&mut buf)?;
			flags(&buf)
		},
		misc_as: r.read_f32::<BigEndian>()?,
		ground: r.read_u16::<BigEndian>()?,
		jumps: r.read_u8()?,
		l_cancel: match r.read_u8()? {
			0 => None,
			l_cancel => Some(frame::LCancel(l_cancel)),
		},
		airborne: r.read_u8()? != 0,
		#[cfg(v2_1)] v2_1: frame_post_v2_1(r)?,
		#[cfg(not(v2_1))] v2_1: match r.is_empty() {
			true => None,
			_ => Some(frame_post_v2_1(r)?),
		},
	})
}

fn frame_post_v0_2(r:&mut &[u8]) -> Result<frame::PostV0_2> {
	Ok(frame::PostV0_2 {
		state_age: r.read_f32::<BigEndian>()?,
		#[cfg(v2_0)] v2_0: frame_post_v2_0(r)?,
		#[cfg(not(v2_0))] v2_0: match r.is_empty() {
			true => None,
			_ => Some(frame_post_v2_0(r)?),
		},
	})
}

fn frame_post(r:&mut &[u8], last_char_states:&mut [CharState; NUM_PORTS]) -> Result<FrameEvent<Post>> {
	let id = FrameId {
		index: r.read_i32::<BigEndian>()?,
		port: r.read_u8()?,
		is_follower: r.read_u8()? != 0,
	};
	debug!("frame::Post: {:?}", id);

	let character = Internal(r.read_u8()?);
	let state = State::from(r.read_u16::<BigEndian>()?, character);
	let position = Position {
		x: r.read_f32::<BigEndian>()?,
		y: r.read_f32::<BigEndian>()?,
	};
	let direction = direction(r.read_f32::<BigEndian>()?)?;
	let damage = r.read_f32::<BigEndian>()?;
	let shield = r.read_f32::<BigEndian>()?;
	let last_attack_landed = {
		let attack = r.read_u8()?;
		match attack {
			0 => None,
			attack => Some(Attack(attack)),
		}
	};
	let combo_count = r.read_u8()?;
	let last_hit_by = r.read_u8()?;
	let stocks = r.read_u8()?;

	#[cfg(v0_2)] let v0_2 = frame_post_v0_2(r)?;
	#[cfg(not(v0_2))] let v0_2 = match r.is_empty() {
		true => None,
		_ => Some(frame_post_v0_2(r)?),
	};

	update_last_char_state(id, character, state, last_char_states);

	Ok(FrameEvent {
		id: id,
		event: Post {
			index: id.index,
			character: character,
			state: state,
			position: position,
			direction: direction,
			damage: damage,
			shield: shield,
			last_attack_landed: last_attack_landed,
			combo_count: combo_count,
			last_hit_by: last_hit_by,
			stocks: stocks,
			v0_2: v0_2,
		},
	})
}

pub trait Handlers {
	fn game_start(&mut self, _:Start) -> Result<()> { Ok(()) }
	fn game_end(&mut self, _:End) -> Result<()> { Ok(()) }
	fn frame_pre(&mut self, _:FrameEvent<Pre>) -> Result<()> { Ok(()) }
	fn frame_post(&mut self, _:FrameEvent<Post>) -> Result<()> { Ok(()) }
	fn metadata(&mut self, _:HashMap<String, ubjson::Object>) -> Result<()> { Ok(()) }
}

fn expect_bytes<R:Read>(r:&mut R, expected:&[u8]) -> Result<()> {
	let mut actual = vec![0; expected.len()];
	r.read_exact(&mut actual)?;
	if expected == actual.as_slice() {
		Ok(())
	} else {
		Err(err!("expected: {:?}, got: {:?}", expected, actual))
	}
}

fn event<R:Read, H:Handlers>(mut r:R, payload_sizes:&HashMap<u8, u16>, last_char_states:&mut [CharState; NUM_PORTS], handlers:&mut H) -> Result<Option<Event>> {
	let code = r.read_u8()?;
	let size = payload_sizes.get(&code).ok_or_else(|| err!("unknown event: {}", code))?;

	let mut buf = vec![0; *size as usize];
	r.read_exact(&mut *buf)?;

	match Event::try_from(code) {
		Ok(event) => match event {
			Event::Payloads => Err(err!("unexpected event: {}", code)),
			Event::GameStart => {
				handlers.game_start(game_start(&mut &*buf)?)?;
				Ok(Some(event))
			},
			Event::GameEnd => {
				handlers.game_end(game_end(&mut &*buf)?)?;
				Ok(Some(event))
			},
			Event::FramePre => {
				handlers.frame_pre(frame_pre(&mut &*buf, last_char_states)?)?;
				Ok(Some(event))
			},
			Event::FramePost => {
				handlers.frame_post(frame_post(&mut &*buf, last_char_states)?)?;
				Ok(Some(event))
			},
		},
		Err(_) => Ok(None),
	}
}

pub fn parse<R:Read, H:Handlers>(mut r:R, handlers:&mut H) -> Result<()> {
	// header ("{U\x03raw[$U#l")
	expect_bytes(&mut r, &[0x7b, 0x55, 0x03, 0x72, 0x61, 0x77, 0x5b, 0x24, 0x55, 0x23, 0x6c])?;

	r.read_u32::<BigEndian>()?; // length of "raw" element (currently unused)

	let payload_sizes = event_payloads(&mut r)?;
	let mut last_char_states = [DEFAULT_CHAR_STATE; NUM_PORTS];

	while event(r.by_ref(), &payload_sizes, &mut last_char_states, handlers)? != Some(Event::GameEnd) {}

	// metadata key & start ("U\x08metadata{")
	expect_bytes(&mut r, &[0x55, 0x08, 0x6d, 0x65, 0x74, 0x61, 0x64, 0x61, 0x74, 0x61, 0x7b])?;

	handlers.metadata(ubjson::parse_map(&mut r)?)?;

	// closing UBJSON brace & end of file ("}")
	expect_bytes(&mut r, &[0x7d])?;

	Ok(())
}

pub fn parse_metadata<R:Read + Seek, H:Handlers>(mut r:R) -> Result<HashMap<String, ubjson::Object>> {
	// header ("{U\x03raw[$U#l")
	expect_bytes(&mut r, &[0x7b, 0x55, 0x03, 0x72, 0x61, 0x77, 0x5b, 0x24, 0x55, 0x23, 0x6c])?;

	let raw_length = r.read_u32::<BigEndian>()?;

	r.seek(SeekFrom::Current(raw_length as i64))?;

	// metadata key & start ("U\x08metadata{")
	expect_bytes(&mut r, &[0x55, 0x08, 0x6d, 0x65, 0x74, 0x61, 0x64, 0x61, 0x74, 0x61, 0x7b])?;

	let metadata = ubjson::parse_map(&mut r)?;

	// closing UBJSON brace & end of file ("}")
	expect_bytes(&mut r, &[0x7d])?;

	Ok(metadata)
}
