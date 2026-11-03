//! Reminders and recurring messages, sent into the conversation that set them
//! when they come due.
//!
//! A schedule is a file under `.dino/schedules`, so it outlives the process
//! that made it. The frontend asks [`Schedules::due`] between submissions and
//! sends what it finds; one that comes due while no session has its
//! conversation open waits for the next that does. A delivery record says the
//! message was handed to the conversation, not that the work it asks for was
//! done, and a delivery is not exactly-once: see [`Schedules::claim`].

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::civil;
use jiff::tz::TimeZone;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::ToolError;
use crate::ToolFuture;
use crate::ToolRuntime;

pub const CREATE: &str = "schedule_create";
pub const LIST: &str = "schedule_list";
pub const UPDATE: &str = "schedule_update";
pub const DELETE: &str = "schedule_delete";
pub const NAMES: [&str; 4] = [CREATE, LIST, UPDATE, DELETE];

/// Where schedules live, relative to the workspace. A write here changes what
/// a later session is sent, so approval treats it as guarded.
pub const DIR: &str = ".dino/schedules";
const DELIVERIES: &str = "deliveries.jsonl";
const MAX_SCHEDULES: usize = 50;
const MAX_MESSAGE_BYTES: usize = 4_000;
const MIN_INTERVAL_SECONDS: i64 = 60;
/// How far ahead a cron expression is searched before it is called one that
/// never comes due: past one February 29th that falls on any weekday.
const CRON_SEARCH_DAYS: u32 = 366 * 8;

/// The time now. A field rather than a call, so tests need not wait.
pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

/// The conversation a session has open, by its saved id. `/clear` and
/// `/resume` change it, so it is shared rather than copied.
pub type Conversation = Arc<Mutex<String>>;

#[must_use]
pub fn system_clock() -> Clock {
    Arc::new(Timestamp::now)
}

/// When a schedule comes due.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Timing {
    Once { at: Timestamp },
    Every { seconds: i64, from: Timestamp },
    Daily { at: String },
    Weekly { days: Vec<String>, at: String },
    Cron { expr: String },
}

impl Timing {
    /// The first time after `after` it comes due, or `None` for a one-time
    /// schedule already past and a cron expression that never matches.
    fn next_after(&self, after: Timestamp, zone: &TimeZone) -> Option<Timestamp> {
        match self {
            Self::Once { at } => (*at > after).then_some(*at),
            Self::Every { seconds, from } => {
                if *from > after {
                    return Some(*from);
                }
                let elapsed = after.as_second() - from.as_second();
                let periods = elapsed / seconds + 1;
                from.checked_add(SignedDuration::from_secs(periods * seconds))
                    .ok()
            }
            Self::Daily { .. } | Self::Weekly { .. } | Self::Cron { .. } => {
                self.cron().ok()?.next_after(after, zone)
            }
        }
    }

    fn cron(&self) -> Result<Cron, ToolError> {
        match self {
            Self::Daily { at } => {
                let (hour, minute) = time_of_day(at)?;
                Cron::parse(&format!("{minute} {hour} * * *"))
            }
            Self::Weekly { days, at } => {
                let (hour, minute) = time_of_day(at)?;
                Cron::parse(&format!("{minute} {hour} * * {}", days.join(",")))
            }
            Self::Cron { expr } => Cron::parse(expr),
            Self::Once { .. } | Self::Every { .. } => {
                Err(ToolError::new("not a calendar schedule"))
            }
        }
    }

    fn recurs(&self) -> bool {
        !matches!(self, Self::Once { .. })
    }

    fn describe(&self, zone: &TimeZone) -> String {
        match self {
            Self::Once { at } => format!("once, at {}", shown(*at, zone)),
            Self::Every { seconds, .. } => format!("every {}", duration_text(*seconds)),
            Self::Daily { at } => format!("every day at {at}"),
            Self::Weekly { days, at } => format!("every {} at {at}", days.join(", ")),
            Self::Cron { expr } => format!("cron `{expr}`"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub message: String,
    pub timing: Timing,
    /// An IANA name, which calendar timings are read in.
    pub timezone: String,
    /// The conversation it is sent into.
    pub conversation: String,
    pub created_at: Timestamp,
    pub next_due: Timestamp,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub deliveries: u32,
    #[serde(default)]
    pub last_delivered: Option<Timestamp>,
}

impl Schedule {
    fn zone(&self) -> TimeZone {
        TimeZone::get(&self.timezone).unwrap_or(TimeZone::UTC)
    }

    /// One line for a listing: id, timing, next due, and what it says.
    #[must_use]
    pub fn summary(&self, conversation: &str) -> String {
        let zone = self.zone();
        let mut line = format!(
            "{}: {} ({})",
            self.id,
            self.timing.describe(&zone),
            self.timezone
        );
        if self.paused {
            line.push_str(", paused");
        } else {
            let _ = write!(line, ", next due {}", shown(self.next_due, &zone));
        }
        if self.deliveries > 0 {
            let _ = write!(line, ", delivered {}", times(self.deliveries));
            if let Some(last) = self.last_delivered {
                let _ = write!(line, ", last {}", shown(last, &zone));
            }
        }
        if self.conversation != conversation {
            let _ = write!(line, ", for conversation {}", self.conversation);
        }
        let _ = write!(
            line,
            ": {}",
            crate::text::capped_head(self.message.trim(), 200)
        );
        line
    }
}

/// One message handed to a conversation. It says nothing of what was done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub schedule: String,
    pub conversation: String,
    pub due_at: Timestamp,
    pub delivered_at: Timestamp,
}

/// The message a due schedule is sent as, framed so the model knows the person
/// did not just type it.
#[must_use]
pub fn delivery_text(schedule: &Schedule, due_at: Timestamp) -> String {
    let zone = schedule.zone();
    format!(
        "[dino: scheduled message {}, set earlier in this conversation ({}, {}), due {}. \
         The person did not just type it.]\n\n{}",
        schedule.id,
        schedule.timing.describe(&zone),
        schedule.timezone,
        shown(due_at, &zone),
        schedule.message.trim()
    )
}

/// The schedules of one workspace, on disk.
#[derive(Debug, Clone)]
pub struct Schedules {
    dir: PathBuf,
}

impl Schedules {
    #[must_use]
    pub fn in_workspace(root: &Path) -> Self {
        Self {
            dir: root.join(DIR),
        }
    }

    /// Every schedule, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error naming a file that cannot be read or parsed: a schedule
    /// skipped in silence would never come due and nobody would know why.
    pub fn all(&self) -> Result<Vec<Schedule>, ToolError> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_error("read", &self.dir, &error)),
        };
        let mut schedules = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let text =
                std::fs::read_to_string(&path).map_err(|error| io_error("read", &path, &error))?;
            let schedule: Schedule = serde_json::from_str(&text).map_err(|error| {
                ToolError::new(format!(
                    "cannot read {}: {error}. Fix or delete it.",
                    path.display()
                ))
            })?;
            schedules.push(schedule);
        }
        schedules.sort_by_key(|schedule| id_number(&schedule.id));
        Ok(schedules)
    }

    /// The schedules due by `now` for `conversation`, earliest first.
    ///
    /// # Errors
    ///
    /// As [`Schedules::all`].
    pub fn due(&self, now: Timestamp, conversation: &str) -> Result<Vec<Schedule>, ToolError> {
        let mut due: Vec<Schedule> = self
            .all()?
            .into_iter()
            .filter(|schedule| {
                !schedule.paused
                    && schedule.next_due <= now
                    && schedule.conversation == conversation
            })
            .collect();
        due.sort_by_key(|schedule| schedule.next_due);
        Ok(due)
    }

    /// The schedules due by `now` for conversations other than this one, which
    /// wait for a session that has theirs open.
    ///
    /// # Errors
    ///
    /// As [`Schedules::all`].
    pub fn waiting_elsewhere(
        &self,
        now: Timestamp,
        conversation: &str,
    ) -> Result<Vec<Schedule>, ToolError> {
        Ok(self
            .all()?
            .into_iter()
            .filter(|schedule| {
                !schedule.paused
                    && schedule.next_due <= now
                    && schedule.conversation != conversation
            })
            .collect())
    }

    /// Takes `due` for delivery: moves it to its next time, or removes a
    /// one-time schedule, and records the delivery. Returns the message to send,
    /// or `None` when it is no longer due as it was seen, because another
    /// session took it or it was changed.
    ///
    /// Recorded before the message is sent, so a process that dies between the
    /// two loses that delivery; two sessions on one conversation can both take
    /// it in the moment between the read and the write. Neither is worth a lock
    /// for a reminder, and neither is hidden.
    ///
    /// # Errors
    ///
    /// Returns an error when the schedule or the record cannot be written.
    pub fn claim(&self, due: &Schedule, now: Timestamp) -> Result<Option<String>, ToolError> {
        let current = match std::fs::read_to_string(self.path(&due.id)?) {
            Ok(text) => serde_json::from_str::<Schedule>(&text).ok(),
            Err(_) => None,
        };
        let Some(mut schedule) = current.filter(|current| {
            !current.paused && current.next_due == due.next_due && current.next_due <= now
        }) else {
            return Ok(None);
        };
        let due_at = schedule.next_due;
        let text = delivery_text(&schedule, due_at);
        // A recurring schedule that was missed for a while is sent once and
        // moves past now, rather than once for every time it missed.
        match schedule
            .timing
            .next_after(now, &schedule.zone())
            .filter(|_| schedule.timing.recurs())
        {
            Some(next) => {
                schedule.next_due = next;
                schedule.deliveries += 1;
                schedule.last_delivered = Some(now);
                self.write(&schedule)?;
            }
            None => self.remove(&schedule.id)?,
        }
        self.record(&Delivery {
            schedule: schedule.id.clone(),
            conversation: schedule.conversation.clone(),
            due_at,
            delivered_at: now,
        })?;
        Ok(Some(text))
    }

    /// Every delivery recorded, oldest first. A line that does not parse is
    /// left out: the record is for reading, and nothing depends on it.
    #[must_use]
    pub fn deliveries(&self) -> Vec<Delivery> {
        std::fs::read_to_string(self.dir.join(DELIVERIES))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn record(&self, delivery: &Delivery) -> Result<(), ToolError> {
        let path = self.dir.join(DELIVERIES);
        let line = serde_json::to_string(delivery)
            .map_err(|error| ToolError::new(format!("cannot encode a delivery: {error}")))?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| writeln!(file, "{line}"))
            .map_err(|error| io_error("write", &path, &error))
    }

    fn write(&self, schedule: &Schedule) -> Result<(), ToolError> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|error| io_error("create", &self.dir, &error))?;
        let path = self.path(&schedule.id)?;
        let text = serde_json::to_string_pretty(schedule)
            .map_err(|error| ToolError::new(format!("cannot encode a schedule: {error}")))?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, text)
            .and_then(|()| std::fs::rename(&temporary, &path))
            .map_err(|error| io_error("write", &path, &error))
    }

    fn remove(&self, id: &str) -> Result<(), ToolError> {
        let path = self.path(id)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("remove", &path, &error)),
        }
    }

    fn path(&self, id: &str) -> Result<PathBuf, ToolError> {
        if id_number(id).is_none() {
            return Err(ToolError::new(format!(
                "{id:?} is not a schedule id; schedule_list shows them, such as s1"
            )));
        }
        Ok(self.dir.join(format!("{id}.json")))
    }

    /// One past the highest id ever used, delivered ones included, so a
    /// record never names two schedules.
    fn next_id(&self, existing: &[Schedule]) -> String {
        let highest = existing
            .iter()
            .map(|schedule| schedule.id.as_str())
            .chain(
                self.deliveries()
                    .iter()
                    .map(|delivery| delivery.schedule.as_str()),
            )
            .filter_map(id_number)
            .max()
            .unwrap_or(0);
        format!("s{}", highest + 1)
    }
}

fn id_number(id: &str) -> Option<u64> {
    id.strip_prefix('s')?.parse().ok()
}

fn io_error(action: &str, path: &Path, error: &std::io::Error) -> ToolError {
    ToolError::new(format!("cannot {action} {}: {error}", path.display()))
}

/// The `schedule_*` tools.
pub struct ScheduleTools {
    store: Schedules,
    conversation: Conversation,
    clock: Clock,
    /// The IANA zone a schedule is read in when it names none.
    zone: String,
}

impl ScheduleTools {
    #[must_use]
    pub fn new(store: Schedules, conversation: Conversation) -> Self {
        Self {
            store,
            conversation,
            clock: system_clock(),
            zone: TimeZone::system().iana_name().unwrap_or("UTC").to_owned(),
        }
    }

    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    #[must_use]
    pub fn with_zone(mut self, zone: impl Into<String>) -> Self {
        self.zone = zone.into();
        self
    }

    fn conversation(&self) -> String {
        self.conversation
            .lock()
            .map(|conversation| conversation.clone())
            .unwrap_or_default()
    }

    fn create(&self, arguments: &Value) -> Result<String, ToolError> {
        let request: CreateRequest = parse(arguments, CREATE)?;
        let message = checked_message(&request.message)?;
        let now = (self.clock)();
        let timezone = request.timezone.unwrap_or_else(|| self.zone.clone());
        let zone = zone_named(&timezone)?;
        let timing = request
            .when
            .timing(now, &zone)?
            .ok_or_else(|| ToolError::new(NO_TIMING))?;
        let next_due = first_due(&timing, now, &zone)?;
        let existing = self.store.all()?;
        if existing.len() >= MAX_SCHEDULES {
            return Err(ToolError::new(format!(
                "this workspace already has {MAX_SCHEDULES} schedules; delete one first"
            )));
        }
        let schedule = Schedule {
            id: self.store.next_id(&existing),
            message,
            timing,
            timezone,
            conversation: self.conversation(),
            created_at: now,
            next_due,
            paused: false,
            deliveries: 0,
            last_delivered: None,
        };
        self.store.write(&schedule)?;
        Ok(format!(
            "Scheduled {}: {} ({}), first due {}. It is sent into this conversation as a \
             message when due, between submissions; if no session has this conversation open \
             then, it waits for the next that does.",
            schedule.id,
            schedule.timing.describe(&zone),
            schedule.timezone,
            shown(next_due, &zone)
        ))
    }

    fn list(&self) -> Result<String, ToolError> {
        let schedules = self.store.all()?;
        if schedules.is_empty() {
            return Ok("No schedules in this workspace.".to_owned());
        }
        let conversation = self.conversation();
        let mut text = String::new();
        for schedule in &schedules {
            text.push_str(&schedule.summary(&conversation));
            text.push('\n');
        }
        Ok(text)
    }

    fn update(&self, arguments: &Value) -> Result<String, ToolError> {
        let request: UpdateRequest = parse(arguments, UPDATE)?;
        let mut schedule = self.find(&request.id)?;
        let now = (self.clock)();
        if let Some(message) = &request.message {
            schedule.message = checked_message(message)?;
        }
        let rezoned = request.timezone.is_some();
        if let Some(timezone) = request.timezone {
            zone_named(&timezone)?;
            schedule.timezone = timezone;
        }
        let zone = schedule.zone();
        let retimed = match request.when.timing(now, &zone)? {
            Some(timing) => {
                schedule.timing = timing;
                true
            }
            None => false,
        };
        let resumed = schedule.paused && request.paused == Some(false);
        if let Some(paused) = request.paused {
            schedule.paused = paused;
        }
        // A schedule resumed after a pause starts from now, not from the times
        // it was paused through.
        if retimed || rezoned || resumed {
            schedule.next_due = first_due(&schedule.timing, now, &zone)?;
        }
        self.store.write(&schedule)?;
        Ok(format!(
            "Updated {}",
            schedule.summary(&self.conversation())
        ))
    }

    fn delete(&self, arguments: &Value) -> Result<String, ToolError> {
        let request: DeleteRequest = parse(arguments, DELETE)?;
        let schedule = self.find(&request.id)?;
        self.store.remove(&schedule.id)?;
        Ok(format!(
            "Deleted {}. Its delivery records are kept.",
            schedule.id
        ))
    }

    fn find(&self, id: &str) -> Result<Schedule, ToolError> {
        self.store.path(id)?;
        self.store
            .all()?
            .into_iter()
            .find(|schedule| schedule.id == id)
            .ok_or_else(|| {
                ToolError::new(format!(
                    "there is no schedule {id}; schedule_list shows them"
                ))
            })
    }

    fn checked(call: &ToolCall) -> Result<(), ToolError> {
        match call.name.as_str() {
            CREATE => parse::<CreateRequest>(&call.arguments, CREATE).map(drop),
            LIST => parse::<ListRequest>(&call.arguments, LIST).map(drop),
            UPDATE => parse::<UpdateRequest>(&call.arguments, UPDATE).map(drop),
            DELETE => parse::<DeleteRequest>(&call.arguments, DELETE).map(drop),
            other => Err(ToolError::new(format!("unknown tool {other:?}"))),
        }
    }
}

const NO_TIMING: &str = "say when: one of delay, at, every, daily, weekly or cron";

/// The timing arguments, of which a create needs exactly one.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct When {
    delay: Option<String>,
    at: Option<String>,
    every: Option<String>,
    daily: Option<String>,
    weekly: Option<String>,
    cron: Option<String>,
}

impl When {
    fn timing(&self, now: Timestamp, zone: &TimeZone) -> Result<Option<Timing>, ToolError> {
        let given = [
            &self.delay,
            &self.at,
            &self.every,
            &self.daily,
            &self.weekly,
            &self.cron,
        ]
        .iter()
        .filter(|value| value.is_some())
        .count();
        if given > 1 {
            return Err(ToolError::new(format!(
                "give only one of delay, at, every, daily, weekly or cron, not {given}"
            )));
        }
        let timing = if let Some(delay) = &self.delay {
            let seconds = duration_seconds(delay)?;
            Timing::Once {
                at: now
                    .checked_add(SignedDuration::from_secs(seconds))
                    .map_err(|error| ToolError::new(format!("delay {delay:?}: {error}")))?,
            }
        } else if let Some(at) = &self.at {
            Timing::Once {
                at: moment(at, now, zone)?,
            }
        } else if let Some(every) = &self.every {
            let seconds = duration_seconds(every)?;
            if seconds < MIN_INTERVAL_SECONDS {
                return Err(ToolError::new("every must be at least a minute"));
            }
            Timing::Every { seconds, from: now }
        } else if let Some(daily) = &self.daily {
            let (hour, minute) = time_of_day(daily)?;
            Timing::Daily {
                at: format!("{hour:02}:{minute:02}"),
            }
        } else if let Some(weekly) = &self.weekly {
            weekly_timing(weekly)?
        } else if let Some(cron) = &self.cron {
            Cron::parse(cron)?;
            Timing::Cron {
                expr: cron.trim().to_owned(),
            }
        } else {
            return Ok(None);
        };
        Ok(Some(timing))
    }
}

macro_rules! with_when {
    ($name:ident { $($field:ident: $kind:ty),* $(,)? }) => {
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct $name {
            $($field: $kind,)*
            #[serde(default)]
            delay: Option<String>,
            #[serde(default)]
            at: Option<String>,
            #[serde(default)]
            every: Option<String>,
            #[serde(default)]
            daily: Option<String>,
            #[serde(default)]
            weekly: Option<String>,
            #[serde(default)]
            cron: Option<String>,
        }
    };
}

with_when!(CreateFields {
    message: String,
    timezone: Option<String>,
});

with_when!(UpdateFields {
    id: String,
    message: Option<String>,
    timezone: Option<String>,
    paused: Option<bool>,
});

struct CreateRequest {
    message: String,
    timezone: Option<String>,
    when: When,
}

struct UpdateRequest {
    id: String,
    message: Option<String>,
    timezone: Option<String>,
    paused: Option<bool>,
    when: When,
}

impl<'de> Deserialize<'de> for CreateRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = CreateFields::deserialize(deserializer)?;
        Ok(Self {
            message: fields.message,
            timezone: fields.timezone,
            when: When {
                delay: fields.delay,
                at: fields.at,
                every: fields.every,
                daily: fields.daily,
                weekly: fields.weekly,
                cron: fields.cron,
            },
        })
    }
}

impl<'de> Deserialize<'de> for UpdateRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = UpdateFields::deserialize(deserializer)?;
        Ok(Self {
            id: fields.id,
            message: fields.message,
            timezone: fields.timezone,
            paused: fields.paused,
            when: When {
                delay: fields.delay,
                at: fields.at,
                every: fields.every,
                daily: fields.daily,
                weekly: fields.weekly,
                cron: fields.cron,
            },
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListRequest {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteRequest {
    id: String,
}

fn parse<T: for<'de> Deserialize<'de>>(arguments: &Value, tool: &str) -> Result<T, ToolError> {
    // A call with no arguments at all arrives as null from some providers.
    let arguments = if arguments.is_null() {
        &json!({})
    } else {
        arguments
    };
    serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::new(format!("invalid {tool} arguments: {error}")))
}

fn checked_message(message: &str) -> Result<String, ToolError> {
    let message = message.trim();
    if message.is_empty() {
        return Err(ToolError::new(
            "the message is empty; say what should be sent",
        ));
    }
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(ToolError::new(format!(
            "the message is {} bytes; keep it under {MAX_MESSAGE_BYTES}",
            message.len()
        )));
    }
    Ok(message.to_owned())
}

fn zone_named(name: &str) -> Result<TimeZone, ToolError> {
    TimeZone::get(name).map_err(|_| {
        ToolError::new(format!(
            "unknown time zone {name:?}; use an IANA name such as Europe/London or UTC"
        ))
    })
}

fn first_due(timing: &Timing, now: Timestamp, zone: &TimeZone) -> Result<Timestamp, ToolError> {
    match timing {
        // A delay of zero is due at once, which `next_after` would call past.
        Timing::Once { at } if *at >= now => Ok(*at),
        Timing::Once { at } => Err(ToolError::new(format!(
            "{} is in the past; it is now {}",
            shown(*at, zone),
            shown(now, zone)
        ))),
        _ => timing
            .next_after(now, zone)
            .ok_or_else(|| ToolError::new("that schedule never comes due")),
    }
}

/// An instant with an offset, a date and time read in `zone`, or a time of day
/// meaning its next occurrence.
fn moment(text: &str, now: Timestamp, zone: &TimeZone) -> Result<Timestamp, ToolError> {
    let text = text.trim();
    if let Ok(instant) = text.parse::<Timestamp>() {
        return Ok(instant);
    }
    if let Ok(datetime) = text.parse::<civil::DateTime>() {
        return zone
            .to_zoned(datetime)
            .map(|zoned| zoned.timestamp())
            .map_err(|error| ToolError::new(format!("at {text:?}: {error}")));
    }
    if let Ok((hour, minute)) = time_of_day(text) {
        let daily = Cron::parse(&format!("{minute} {hour} * * *"))?;
        // Minus a second, so a time of day that is this very minute is today.
        let after = now.checked_sub(SignedDuration::from_secs(1)).unwrap_or(now);
        return daily
            .next_after(after, zone)
            .ok_or_else(|| ToolError::new(format!("at {text:?} never comes")));
    }
    Err(ToolError::new(format!(
        "at {text:?} is not a time: use 2026-11-04T09:00 in the schedule's zone, \
         2026-11-04T09:00Z with an offset, or a time of day such as 15:30"
    )))
}

/// `HH:MM`, on a 24-hour clock.
fn time_of_day(text: &str) -> Result<(u8, u8), ToolError> {
    let invalid = || {
        ToolError::new(format!(
            "{text:?} is not a time of day; use HH:MM on a 24-hour clock, such as 09:30"
        ))
    };
    let (hour, minute) = text.trim().split_once(':').ok_or_else(invalid)?;
    let hour: u8 = hour.parse().map_err(|_| invalid())?;
    let minute: u8 = minute.parse().map_err(|_| invalid())?;
    if hour > 23 || minute > 59 || text.trim().len() > 5 {
        return Err(invalid());
    }
    Ok((hour, minute))
}

/// `mon,thu 09:00`, or `weekdays 09:00`.
fn weekly_timing(text: &str) -> Result<Timing, ToolError> {
    let invalid = || {
        ToolError::new(format!(
            "weekly {text:?} needs days then a time, such as \"mon,thu 09:00\" or \
             \"weekdays 08:30\""
        ))
    };
    let (days, at) = text
        .trim()
        .rsplit_once(char::is_whitespace)
        .ok_or_else(invalid)?;
    let (hour, minute) = time_of_day(at)?;
    let mut numbers = Vec::new();
    for day in days
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|day| !day.is_empty())
    {
        match day.to_ascii_lowercase().as_str() {
            "weekdays" => numbers.extend(1..=5),
            "weekends" => numbers.extend([6, 0]),
            other => numbers.push(weekday_number(other).ok_or_else(invalid)?),
        }
    }
    if numbers.is_empty() {
        return Err(invalid());
    }
    // Monday first, as a person reads a week.
    numbers.sort_by_key(|day| (day + 6) % 7);
    numbers.dedup();
    Ok(Timing::Weekly {
        days: numbers
            .into_iter()
            .map(|day| DAY_NAMES[usize::from(day)].to_owned())
            .collect(),
        at: format!("{hour:02}:{minute:02}"),
    })
}

const DAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
const MONTH_NAMES: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

fn weekday_number(name: &str) -> Option<u8> {
    let name = name.to_ascii_lowercase();
    let short = name.get(..3)?;
    let index = DAY_NAMES.iter().position(|day| *day == short)?;
    let full = [
        "sunday",
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
    ][index];
    (name.len() == 3 || full.starts_with(&name)).then(|| u8::try_from(index).unwrap_or(0))
}

/// `20m`, `1h30m`, `90s`, `2 days`: a sum of amounts and units.
fn duration_seconds(text: &str) -> Result<i64, ToolError> {
    let invalid = || {
        ToolError::new(format!(
            "{text:?} is not a duration; use amounts with units, such as 20m, 1h30m or 2d"
        ))
    };
    let mut total: i64 = 0;
    let mut rest = text.trim();
    if rest.is_empty() {
        return Err(invalid());
    }
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits == 0 {
            return Err(invalid());
        }
        let amount: i64 = rest[..digits].parse().map_err(|_| invalid())?;
        rest = rest[digits..].trim_start();
        let letters = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let unit = match rest[..letters].to_ascii_lowercase().as_str() {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
            "d" | "day" | "days" => 86_400,
            "w" | "week" | "weeks" => 7 * 86_400,
            _ => return Err(invalid()),
        };
        total = amount
            .checked_mul(unit)
            .and_then(|seconds| total.checked_add(seconds))
            .ok_or_else(invalid)?;
        rest = rest[letters..].trim_start_matches([' ', ',']);
        rest = rest.strip_prefix("and ").unwrap_or(rest);
    }
    Ok(total)
}

fn duration_text(seconds: i64) -> String {
    let units = [(86_400, "d"), (3_600, "h"), (60, "m"), (1, "s")];
    let mut text = String::new();
    let mut left = seconds;
    for (size, unit) in units {
        if left >= size {
            let _ = write!(text, "{}{unit}", left / size);
            left %= size;
        }
    }
    text
}

fn times(count: u32) -> String {
    match count {
        1 => "once".to_owned(),
        2 => "twice".to_owned(),
        n => format!("{n} times"),
    }
}

/// An instant as a person in `zone` reads it.
fn shown(instant: Timestamp, zone: &TimeZone) -> String {
    instant
        .to_zoned(zone.clone())
        .strftime("%Y-%m-%d %H:%M %Z")
        .to_string()
}

/// A five-field cron expression: minute, hour, day of month, month, day of
/// week, read in the schedule's zone.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    /// Whether the day-of-month and day-of-week fields were `*`. When both are
    /// restricted, cron runs on a day either matches.
    any_day: bool,
    any_weekday: bool,
}

impl Cron {
    fn parse(expression: &str) -> Result<Self, ToolError> {
        let expanded = match expression.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            other => other,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields[..] else {
            return Err(ToolError::new(format!(
                "cron {expression:?} needs five fields: minute hour day-of-month month \
                 day-of-week, such as \"0 9 * * 1-5\""
            )));
        };
        let field = |text: &str, name: &str, low: u8, high: u8, names: &[&str]| {
            cron_field(text, low, high, names).map_err(|why| {
                ToolError::new(format!("cron {expression:?}: the {name} field {why}"))
            })
        };
        let weekdays = field(weekday, "day-of-week", 0, 7, &DAY_NAMES)?;
        Ok(Self {
            minutes: field(minute, "minute", 0, 59, &[])?,
            hours: u32::try_from(field(hour, "hour", 0, 23, &[])?).unwrap_or(0),
            days: u32::try_from(field(day, "day-of-month", 1, 31, &[])?).unwrap_or(0),
            months: u16::try_from(field(month, "month", 1, 12, &MONTH_NAMES)? >> 1).unwrap_or(0),
            // Seven is Sunday as well as zero.
            weekdays: u8::try_from((weekdays | (weekdays >> 7)) & 0x7f).unwrap_or(0),
            any_day: day == "*",
            any_weekday: weekday == "*",
        })
    }

    fn matches_day(&self, date: civil::Date) -> bool {
        let month = self.months & (1 << (date.month() - 1)) != 0;
        let day = self.days & (1 << date.day()) != 0;
        let weekday = self.weekdays & (1 << date.weekday().to_sunday_zero_offset()) != 0;
        let either = if self.any_day || self.any_weekday {
            day && weekday
        } else {
            day || weekday
        };
        month && either
    }

    /// The first matching minute after `after`, read in `zone`. A time a clock
    /// change skips is taken as the time after the change, and one it repeats
    /// comes due once.
    fn next_after(&self, after: Timestamp, zone: &TimeZone) -> Option<Timestamp> {
        let start = after.to_zoned(zone.clone()).datetime();
        let mut date = start.date();
        for _ in 0..CRON_SEARCH_DAYS {
            if self.matches_day(date) {
                let today = date == start.date();
                for hour in (0..24i8).filter(|hour| self.hours & (1 << hour) != 0) {
                    if today && hour < start.hour() {
                        continue;
                    }
                    for minute in (0..60i8).filter(|minute| self.minutes & (1 << minute) != 0) {
                        if today && hour == start.hour() && minute < start.minute() {
                            continue;
                        }
                        let candidate =
                            zone.to_zoned(date.at(hour, minute, 0, 0)).ok()?.timestamp();
                        if candidate > after {
                            return Some(candidate);
                        }
                    }
                }
            }
            date = date.tomorrow().ok()?;
        }
        None
    }
}

/// One cron field as a bit set, bit `n` for value `n`.
fn cron_field(text: &str, low: u8, high: u8, names: &[&str]) -> Result<u64, String> {
    let value = |part: &str| -> Result<u8, String> {
        if let Some(index) = names
            .iter()
            .position(|name| name.eq_ignore_ascii_case(part))
        {
            return Ok(u8::try_from(index).unwrap_or(0) + low);
        }
        let number: u8 = part
            .parse()
            .map_err(|_| format!("has {part:?}, which is not a number"))?;
        if number < low || number > high {
            return Err(format!("has {number}, outside {low}-{high}"));
        }
        Ok(number)
    };
    let mut bits = 0u64;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u8 =
                    step.parse().ok().filter(|step| *step > 0).ok_or_else(|| {
                        format!("has step {step:?}, which is not a positive number")
                    })?;
                (range, step)
            }
            None => (part, 1),
        };
        let (first, last) = if range == "*" {
            (low, high)
        } else if let Some((first, last)) = range.split_once('-') {
            (value(first)?, value(last)?)
        } else {
            let first = value(range)?;
            // `5/15` means from 5 to the end in steps of 15.
            (first, if part.contains('/') { high } else { first })
        };
        if first > last {
            return Err(format!("has the range {range}, which runs backwards"));
        }
        for number in (first..=last).step_by(usize::from(step)) {
            bits |= 1 << number;
        }
    }
    Ok(bits)
}

impl ToolRuntime for ScheduleTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let timing = json!({
            "delay": {"type": "string", "minLength": 1, "description": "Once, after this long, such as 20m, 1h30m or 2d."},
            "at": {"type": "string", "minLength": 1, "description": "Once, at 2026-11-04T09:00 in the zone, at an instant with an offset, or at the next 15:30."},
            "every": {"type": "string", "minLength": 1, "description": "Repeatedly, this long apart, from now: 30m, 2h, 1d. At least a minute."},
            "daily": {"type": "string", "minLength": 1, "description": "Every day at HH:MM in the zone."},
            "weekly": {"type": "string", "minLength": 1, "description": "Days then a time in the zone: \"mon,thu 09:00\" or \"weekdays 08:30\"."},
            "cron": {"type": "string", "minLength": 1, "description": "Five fields, minute hour day-of-month month day-of-week, in the zone: \"0 9 * * 1-5\"."},
            "timezone": {"type": "string", "minLength": 1, "description": "An IANA zone such as Europe/London. This machine's zone when omitted."}
        });
        let with = |extra: Value| {
            let mut properties = timing.clone();
            if let (Some(properties), Value::Object(extra)) = (properties.as_object_mut(), extra) {
                properties.extend(extra);
            }
            properties
        };
        vec![
            ToolDefinition {
                name: CREATE.into(),
                description: "Schedule a message to be sent into this conversation later, \
                              once or repeatedly, as a reminder or a recurring task. Give the \
                              message and exactly one of delay, at, every, daily, weekly or \
                              cron. It is sent between submissions when due; a conversation \
                              no session has open gets it when one next does. Only when the \
                              person asks for something later or repeated."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": with(json!({
                        "message": {"type": "string", "minLength": 1, "description": "What is sent when it comes due, written as the request it should be."}
                    })),
                    "required": ["message"],
                    "additionalProperties": false
                }),
            },
            ToolDefinition {
                name: LIST.into(),
                description: "List this workspace's schedules: id, timing, next due, how often \
                              delivered, and the message. Delivered means sent into the \
                              conversation, not that the work was done."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            ToolDefinition {
                name: UPDATE.into(),
                description: "Change a schedule by id: its message, its timing (one of delay, \
                              at, every, daily, weekly or cron), its zone, or pause it with \
                              paused true and resume it with false."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": with(json!({
                        "id": {"type": "string", "minLength": 1},
                        "message": {"type": "string", "minLength": 1},
                        "paused": {"type": "boolean"}
                    })),
                    "required": ["id"],
                    "additionalProperties": false
                }),
            },
            ToolDefinition {
                name: DELETE.into(),
                description: "Delete a schedule by id, so it is never sent again.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"id": {"type": "string", "minLength": 1}},
                    "required": ["id"],
                    "additionalProperties": false
                }),
            },
        ]
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        Self::checked(call)?;
        Ok(if call.name == LIST {
            ToolAction::read_only()
        } else {
            ToolAction::mutating(DIR)
        })
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let result = match call.name.as_str() {
            CREATE => self.create(&call.arguments),
            LIST => parse::<ListRequest>(&call.arguments, LIST).and_then(|_| self.list()),
            UPDATE => self.update(&call.arguments),
            DELETE => self.delete(&call.arguments),
            other => Err(ToolError::new(format!("unknown tool {other:?}"))),
        }
        .map(ToolOutput::success);
        Box::pin(async move { result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("dino-schedule-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn instant(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    /// Tools on a clock the test moves, in London, for conversation `c1`.
    struct Fixture {
        tools: ScheduleTools,
        store: Schedules,
        now: Arc<Mutex<Timestamp>>,
        conversation: Conversation,
    }

    fn fixture(name: &str, now: &str) -> Fixture {
        let store = Schedules::in_workspace(&scratch(name));
        let now = Arc::new(Mutex::new(instant(now)));
        let clock_now = Arc::clone(&now);
        let conversation: Conversation = Arc::new(Mutex::new("c1".to_owned()));
        let tools = ScheduleTools::new(store.clone(), Arc::clone(&conversation))
            .with_clock(Arc::new(move || *clock_now.lock().unwrap()))
            .with_zone("Europe/London");
        Fixture {
            tools,
            store,
            now,
            conversation,
        }
    }

    impl Fixture {
        async fn call(&self, name: &str, arguments: Value) -> Result<String, ToolError> {
            let call = ToolCall {
                id: "call-1".into(),
                name: name.into(),
                arguments,
            };
            self.tools.classify(&call)?;
            self.tools.execute(&call).await.map(|output| output.content)
        }

        fn at(&self, text: &str) -> Timestamp {
            let instant = instant(text);
            *self.now.lock().unwrap() = instant;
            instant
        }
    }

    #[test]
    fn durations_add_up_their_parts() {
        assert_eq!(duration_seconds("20m").unwrap(), 1_200);
        assert_eq!(duration_seconds("1h30m").unwrap(), 5_400);
        assert_eq!(duration_seconds("2 days").unwrap(), 172_800);
        assert_eq!(duration_seconds("1 hour and 5 minutes").unwrap(), 3_900);
        assert!(duration_seconds("soon").is_err());
        assert!(duration_seconds("20").is_err());
    }

    #[test]
    fn a_cron_expression_comes_due_at_its_next_matching_minute() {
        let cron = Cron::parse("0 9 * * 1-5").unwrap();
        // Friday 3 April 2026, 10:00 in London (BST): next is Monday at 09:00.
        let next = cron
            .next_after(
                instant("2026-04-03T09:00:00Z"),
                &TimeZone::get("Europe/London").unwrap(),
            )
            .unwrap();
        assert_eq!(next, instant("2026-04-06T08:00:00Z"));
    }

    #[test]
    fn steps_lists_and_names_are_read_as_cron_reads_them() {
        let cron = Cron::parse("*/15 8-10 * jan,jul mon").unwrap();
        assert_eq!(cron.minutes, (1 << 0) | (1 << 15) | (1 << 30) | (1 << 45));
        assert_eq!(cron.hours, (1 << 8) | (1 << 9) | (1 << 10));
        assert_eq!(cron.months, 1 | (1 << 6));
        assert_eq!(cron.weekdays, 1 << 1);
        assert_eq!(Cron::parse("0 0 * * 7").unwrap().weekdays, 1);
        assert!(Cron::parse("0 9 * *").is_err());
        assert!(Cron::parse("61 9 * * *").is_err());
    }

    #[test]
    fn a_day_of_month_and_a_day_of_week_together_mean_either() {
        let cron = Cron::parse("0 12 1 * fri").unwrap();
        let utc = TimeZone::UTC;
        // Thursday 1 October 2026 matches the day of month; Friday 2nd the weekday.
        let first = cron
            .next_after(instant("2026-09-30T13:00:00Z"), &utc)
            .unwrap();
        assert_eq!(first, instant("2026-10-01T12:00:00Z"));
        let second = cron.next_after(first, &utc).unwrap();
        assert_eq!(second, instant("2026-10-02T12:00:00Z"));
    }

    #[test]
    fn a_daily_time_follows_the_zone_across_a_clock_change() {
        let london = TimeZone::get("Europe/London").unwrap();
        let daily = Timing::Daily { at: "09:00".into() };
        // Clocks go back on 25 October 2026: 08:00 UTC before, 09:00 UTC after.
        let before = daily
            .next_after(instant("2026-10-24T09:00:00Z"), &london)
            .unwrap();
        assert_eq!(before, instant("2026-10-25T09:00:00Z"));
        let earlier = daily
            .next_after(instant("2026-10-23T09:00:00Z"), &london)
            .unwrap();
        assert_eq!(earlier, instant("2026-10-24T08:00:00Z"));
    }

    #[test]
    fn a_time_the_clocks_skip_comes_due_after_the_change() {
        let london = TimeZone::get("Europe/London").unwrap();
        let cron = Cron::parse("30 1 * * *").unwrap();
        // 01:30 does not exist on 29 March 2026 in London; 02:30 BST is 01:30 UTC.
        let next = cron
            .next_after(instant("2026-03-28T12:00:00Z"), &london)
            .unwrap();
        assert_eq!(next, instant("2026-03-29T01:30:00Z"));
    }

    #[test]
    fn a_cron_expression_that_never_matches_is_refused() {
        let never = Timing::Cron {
            expr: "0 0 31 2 *".into(),
        };
        assert!(first_due(&never, instant("2026-01-01T00:00:00Z"), &TimeZone::UTC).is_err());
    }

    #[test]
    fn an_interval_missed_for_hours_moves_past_now_on_its_own_grid() {
        let every = Timing::Every {
            seconds: 1_800,
            from: instant("2026-11-03T09:00:00Z"),
        };
        let next = every
            .next_after(instant("2026-11-03T12:10:00Z"), &TimeZone::UTC)
            .unwrap();
        assert_eq!(next, instant("2026-11-03T12:30:00Z"));
    }

    #[test]
    fn weekly_days_are_named_monday_first() {
        assert_eq!(
            weekly_timing("thu,Monday sun 07:05").unwrap(),
            Timing::Weekly {
                days: vec!["mon".into(), "thu".into(), "sun".into()],
                at: "07:05".into()
            }
        );
        assert!(weekly_timing("09:00").is_err());
        assert!(weekly_timing("someday 09:00").is_err());
    }

    #[tokio::test]
    async fn a_reminder_is_due_after_its_delay_and_only_for_its_conversation() {
        let fixture = fixture("delay", "2026-11-03T10:00:00Z");
        let said = fixture
            .call(
                CREATE,
                json!({"message": "check the deploy", "delay": "20m"}),
            )
            .await
            .unwrap();
        assert!(said.starts_with("Scheduled s1: once, at 2026-11-03 10:20 GMT"));

        let early = fixture.at("2026-11-03T10:19:59Z");
        assert!(fixture.store.due(early, "c1").unwrap().is_empty());
        let due = fixture.at("2026-11-03T10:20:00Z");
        assert_eq!(fixture.store.due(due, "c1").unwrap().len(), 1);
        assert!(fixture.store.due(due, "c2").unwrap().is_empty());
        assert_eq!(fixture.store.waiting_elsewhere(due, "c2").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_claimed_reminder_is_recorded_once_and_gone() {
        let fixture = fixture("claim", "2026-11-03T10:00:00Z");
        fixture
            .call(
                CREATE,
                json!({"message": "check the deploy", "delay": "1m"}),
            )
            .await
            .unwrap();
        let now = fixture.at("2026-11-03T10:05:00Z");
        let due = fixture.store.due(now, "c1").unwrap().remove(0);

        let text = fixture.store.claim(&due, now).unwrap().unwrap();
        assert!(text.contains("scheduled message s1"));
        assert!(text.contains("The person did not just type it."));
        assert!(text.ends_with("check the deploy"));
        assert_eq!(fixture.store.claim(&due, now).unwrap(), None);
        assert!(fixture.store.all().unwrap().is_empty());
        assert_eq!(
            fixture.store.deliveries(),
            vec![Delivery {
                schedule: "s1".into(),
                conversation: "c1".into(),
                due_at: instant("2026-11-03T10:01:00Z"),
                delivered_at: now,
            }]
        );
    }

    #[tokio::test]
    async fn a_recurring_schedule_is_sent_once_for_a_missed_stretch_and_moves_on() {
        let fixture = fixture("recurring", "2026-11-03T08:00:00Z");
        fixture
            .call(CREATE, json!({"message": "standup notes", "daily": "9:00"}))
            .await
            .unwrap();
        // Three days pass with no session open.
        let now = fixture.at("2026-11-06T12:00:00Z");
        let due = fixture.store.due(now, "c1").unwrap();
        assert_eq!(due.len(), 1);
        fixture.store.claim(&due[0], now).unwrap().unwrap();

        let schedule = fixture.store.all().unwrap().remove(0);
        assert_eq!(schedule.next_due, instant("2026-11-07T09:00:00Z"));
        assert_eq!(schedule.deliveries, 1);
        assert_eq!(fixture.store.deliveries().len(), 1);
    }

    #[tokio::test]
    async fn a_paused_schedule_is_not_due_and_resumes_from_now() {
        let fixture = fixture("pause", "2026-11-03T08:00:00Z");
        fixture
            .call(CREATE, json!({"message": "run the audit", "every": "1h"}))
            .await
            .unwrap();
        fixture
            .call(UPDATE, json!({"id": "s1", "paused": true}))
            .await
            .unwrap();
        let later = fixture.at("2026-11-03T11:30:00Z");
        assert!(fixture.store.due(later, "c1").unwrap().is_empty());

        fixture
            .call(UPDATE, json!({"id": "s1", "paused": false}))
            .await
            .unwrap();
        let schedule = fixture.store.all().unwrap().remove(0);
        assert_eq!(schedule.next_due, instant("2026-11-03T12:00:00Z"));
    }

    #[tokio::test]
    async fn list_names_the_conversation_of_a_schedule_set_elsewhere() {
        let fixture = fixture("list", "2026-11-03T08:00:00Z");
        fixture
            .call(
                CREATE,
                json!({"message": "release notes", "weekly": "fri 16:00", "timezone": "America/New_York"}),
            )
            .await
            .unwrap();
        *fixture.conversation.lock().unwrap() = "c2".to_owned();
        let listed = fixture.call(LIST, json!({})).await.unwrap();
        assert_eq!(
            listed,
            "s1: every fri at 16:00 (America/New_York), next due 2026-11-06 16:00 EST, \
             for conversation c1: release notes\n"
        );
    }

    #[tokio::test]
    async fn ids_are_not_reused_after_a_delivery_or_a_delete() {
        let fixture = fixture("ids", "2026-11-03T08:00:00Z");
        for message in ["one", "two"] {
            fixture
                .call(CREATE, json!({"message": message, "delay": "1m"}))
                .await
                .unwrap();
        }
        let now = fixture.at("2026-11-03T08:01:00Z");
        for due in fixture.store.due(now, "c1").unwrap() {
            fixture.store.claim(&due, now).unwrap();
        }
        let said = fixture
            .call(CREATE, json!({"message": "three", "delay": "1m"}))
            .await
            .unwrap();
        assert!(said.starts_with("Scheduled s3"), "{said}");
        fixture.call(DELETE, json!({"id": "s3"})).await.unwrap();
        assert!(fixture.call(DELETE, json!({"id": "s3"})).await.is_err());
        assert!(fixture.call(DELETE, json!({"id": "../x"})).await.is_err());
    }

    #[tokio::test]
    async fn a_create_needs_exactly_one_timing_and_a_time_not_past() {
        let fixture = fixture("refuse", "2026-11-03T08:00:00Z");
        let none = fixture.call(CREATE, json!({"message": "x"})).await;
        assert_eq!(none.unwrap_err().to_string(), NO_TIMING);
        let two = fixture
            .call(
                CREATE,
                json!({"message": "x", "delay": "1m", "daily": "09:00"}),
            )
            .await;
        assert!(two.unwrap_err().to_string().contains("only one"));
        let past = fixture
            .call(CREATE, json!({"message": "x", "at": "2026-11-02T09:00"}))
            .await;
        assert!(past.unwrap_err().to_string().contains("in the past"));
        let zone = fixture
            .call(
                CREATE,
                json!({"message": "x", "delay": "1m", "timezone": "Mars/Base"}),
            )
            .await;
        assert!(zone.unwrap_err().to_string().contains("IANA"));
    }

    #[tokio::test]
    async fn a_time_of_day_means_its_next_occurrence() {
        let fixture = fixture("time", "2026-11-03T16:00:00Z");
        let said = fixture
            .call(CREATE, json!({"message": "x", "at": "15:30"}))
            .await
            .unwrap();
        assert!(said.contains("2026-11-04 15:30 GMT"), "{said}");
    }

    #[test]
    fn only_listing_runs_without_approval() {
        let store = Schedules::in_workspace(&scratch("classify"));
        let tools = ScheduleTools::new(store, Arc::new(Mutex::new("c1".into())));
        let call = |name: &str, arguments: Value| ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments,
        };
        assert_eq!(
            tools.classify(&call(LIST, json!({}))).unwrap(),
            ToolAction::read_only()
        );
        assert_eq!(
            tools
                .classify(&call(CREATE, json!({"message": "x", "delay": "1m"})))
                .unwrap(),
            ToolAction::mutating(DIR)
        );
        assert!(tools.classify(&call(DELETE, json!({}))).is_err());
    }
}
