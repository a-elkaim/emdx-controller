//! The content manifest the display downloads after MDC `0xC7/0x53`, exactly as the
//! Android ePaper app writes it (`ServerContentInfo` and its kotlinx serializers):
//! same keys, same order, same constants.

use serde::Serialize;

/// Where the display keeps downloaded content (the app's `EPD_FILE_PATH`)
const CONTENTS_DIR: &str = "/home/owner/content/Downloads/vxtplayer/epaper/mobile/contents/";

/// The app's `context.getPackageName()`
pub const PROGRAM_ID: &str = "com.samsung.android.ePaper";

/// The `duration` the app gives an image shown alone (24 h in milliseconds)
pub const SINGLE_IMAGE_MILLIS: u64 = 86_400_000;

/// What a manifest describes (the app's `ContentType`, by its enum name)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    Image,
    Playlist,
    Schedule,
}

impl ContentType {
    /// The value of `content_type`, in the manifest and in its URL
    pub fn name(self) -> &'static str {
        match self {
            Self::Image => "ImageContent",
            Self::Playlist => "PlaylistContent",
            Self::Schedule => "ScheduleContent",
        }
    }
}

/// A content id as the app makes them: `content_info_<uuid>`
pub fn new_content_id() -> String {
    format!("content_info_{}", uuid::Uuid::new_v4())
}

/// One image, as the display downloads it
#[derive(Debug, Clone)]
pub struct Slide {
    pub image_url: String,
    /// Its own content id: the display keeps each image in `contents/<file_id>/`
    pub file_id: String,
    pub file_name: String,
    pub file_size: usize,
}

/// `ServerContentInfo`, in the serializer's field order
#[derive(Serialize)]
struct Manifest<'a> {
    id: &'a str,
    program_id: &'a str,
    deploy_type: &'a str,
    version: u32,
    name: &'a str,
    content_type: &'a str,
    schedule: Vec<Block>,
    create_time: &'a str,
}

/// `ServerScheduleContent`
#[derive(Serialize)]
struct Block {
    start_date: &'static str,
    stop_date: &'static str,
    start_time: String,
    contents: Vec<Child>,
}

/// `ServerScheduleChildContent`
#[derive(Serialize)]
struct Child {
    file_id: String,
    file_size: String,
    file_name: String,
    file_path: String,
    image_url: String,
    duration: u64,
}

fn child(s: &Slide, duration_ms: u64) -> Child {
    Child {
        file_id: s.file_id.clone(),
        file_size: s.file_size.to_string(),
        file_name: s.file_name.clone(),
        file_path: format!("{CONTENTS_DIR}{}/{}", s.file_id, s.file_name),
        image_url: s.image_url.clone(),
        duration: duration_ms,
    }
}

/// A block valid every day (the app's constant dates) from `start_time`
fn block(start_time: String, contents: Vec<Child>) -> Block {
    Block {
        start_date: "1970-01-01",
        stop_date: "2999-12-31",
        start_time,
        contents,
    }
}

fn render(
    content_id: &str,
    name: &str,
    content_type: ContentType,
    schedule: Vec<Block>,
    create_time: &str,
) -> String {
    serde_json::to_string(&Manifest {
        id: content_id,
        program_id: PROGRAM_ID,
        deploy_type: "MOBILE",
        version: 1,
        name,
        content_type: content_type.name(),
        schedule,
        create_time,
    })
    .expect("a manifest is plain strings and numbers")
}

/// The manifest of a single image (`ImageContent`): one block from midnight with
/// that image for 24 h. Its `id` is the image's own content id.
pub fn image_manifest(name: &str, slide: &Slide, create_time: &str) -> String {
    render(
        &slide.file_id,
        name,
        ContentType::Image,
        vec![block(
            "00:00:00".into(),
            vec![child(slide, SINGLE_IMAGE_MILLIS)],
        )],
        create_time,
    )
}

/// How long each slide stays on screen. The ePaper app allows 5 minutes to 24 hours
/// (presets 4, 8, 16 and 24 h, or a custom time; a developer setting lowers the
/// minimum to 1 minute), and writes it to every slide in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlideInterval(u32);

impl SlideInterval {
    pub const MIN_MINUTES: u32 = 5;
    pub const MAX_MINUTES: u32 = 24 * 60;

    pub fn from_minutes(minutes: u32) -> Result<Self, String> {
        if (Self::MIN_MINUTES..=Self::MAX_MINUTES).contains(&minutes) {
            Ok(Self(minutes))
        } else {
            Err(format!(
                "the slideshow interval must be between 5 minutes and 24 hours (got {minutes} minutes)"
            ))
        }
    }

    /// Minutes (`30`, `30min`), hours (`2h`) or hours and minutes (`1:30`, `1h30`)
    pub fn parse(value: &str) -> Result<Self, String> {
        let v = value.trim().to_ascii_lowercase().replace(' ', "");
        let bad = || {
            format!(
                "invalid interval {value:?}: minutes (30), hours (2h) or hours and minutes \
                 (1:30, 1h30), from 5 minutes to 24 hours"
            )
        };
        let minutes = match v.split_once([':', 'h']) {
            Some((h, m)) => {
                let h: u32 = h.parse().map_err(|_| bad())?;
                let m = m.trim_end_matches("min").trim_end_matches('m');
                let m: u32 = if m.is_empty() {
                    0
                } else {
                    m.parse().map_err(|_| bad())?
                };
                if m > 59 {
                    return Err(bad());
                }
                h * 60 + m
            }
            None => v
                .trim_end_matches("min")
                .trim_end_matches('m')
                .parse()
                .map_err(|_| bad())?,
        };
        Self::from_minutes(minutes)
    }

    pub fn minutes(self) -> u32 {
        self.0
    }

    /// What the manifest carries per slide (the app: `interval * 60 * 1000`)
    pub fn millis(self) -> u64 {
        u64::from(self.0) * 60_000
    }
}

impl std::fmt::Display for SlideInterval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.0 / 60, self.0 % 60) {
            (0, m) => write!(f, "{m} minutes"),
            (h, 0) => write!(f, "{h} h"),
            (h, m) => write!(f, "{h} h {m} min"),
        }
    }
}

/// The manifest of a slideshow (the app's `PlaylistContent`): one block from midnight,
/// one entry per slide in `contents`, shown in that order, each for `interval`. The
/// display then runs it on its own.
pub fn playlist_manifest(
    content_id: &str,
    name: &str,
    slides: &[Slide],
    interval: SlideInterval,
    create_time: &str,
) -> String {
    let contents = slides.iter().map(|s| child(s, interval.millis())).collect();
    render(
        content_id,
        name,
        ContentType::Playlist,
        vec![block("00:00:00".into(), contents)],
        create_time,
    )
}

/// The time of day a schedule entry starts at. The app keeps it in minutes since
/// midnight and writes it to the manifest as `HH:MM:00`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StartTime(u16);

impl StartTime {
    pub fn from_minutes(minutes: u32) -> Result<Self, String> {
        if minutes < 24 * 60 {
            Ok(Self(minutes as u16))
        } else {
            Err(format!(
                "a start time must be before 24:00 (got {minutes} minutes after midnight)"
            ))
        }
    }

    /// `8:00`, `08:00`, `8h`, `8h30` (24-hour clock)
    pub fn parse(value: &str) -> Result<Self, String> {
        let v = value.trim().to_ascii_lowercase();
        let bad = || format!("invalid time {value:?}: use HH:MM from 00:00 to 23:59");
        let (h, m) = v.split_once([':', 'h']).ok_or_else(bad)?;
        let digits =
            |t: &str| !t.is_empty() && t.len() <= 2 && t.bytes().all(|b| b.is_ascii_digit());
        if !digits(h) || !(m.is_empty() && v.contains('h') || digits(m)) {
            return Err(bad());
        }
        let h: u32 = h.parse().map_err(|_| bad())?;
        let m: u32 = if m.is_empty() {
            0
        } else {
            m.parse().map_err(|_| bad())?
        };
        if h > 23 || m > 59 {
            return Err(bad());
        }
        Ok(Self((h * 60 + m) as u16))
    }

    pub fn minutes(self) -> u32 {
        u32::from(self.0)
    }

    /// The manifest's `start_time`
    pub fn manifest(self) -> String {
        format!("{self}:00")
    }
}

impl std::fmt::Display for StartTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02}:{:02}", self.0 / 60, self.0 % 60)
    }
}

/// One entry of a schedule: from `start`, show `slide`
#[derive(Debug, Clone)]
pub struct ScheduleBlock {
    pub start: StartTime,
    pub slide: Slide,
}

/// The manifest of a schedule (the app's `ScheduleContent`): one `schedule` block
/// per start time, in time order, each valid every day (the app's constant dates)
/// and holding one image. An entry has no duration of its own: it lasts until the
/// next one starts, and the image carries the app's 24 h.
pub fn schedule_manifest(
    content_id: &str,
    name: &str,
    blocks: &[ScheduleBlock],
    create_time: &str,
) -> String {
    // The app collects the blocks in a map sorted by start time
    let mut blocks: Vec<&ScheduleBlock> = blocks.iter().collect();
    blocks.sort_by_key(|b| b.start);
    let schedule = blocks
        .iter()
        .map(|b| {
            block(
                b.start.manifest(),
                vec![child(&b.slide, SINGLE_IMAGE_MILLIS)],
            )
        })
        .collect();
    render(
        content_id,
        name,
        ContentType::Schedule,
        schedule,
        create_time,
    )
}

/// `yyyy-MM-dd HH:mm:ss` in local time, like the app's `SimpleDateFormat`.
pub fn create_time_now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_manifest_is_the_apps_json() {
        let slide = Slide {
            image_url: "https://1.2.3.4:6869/image?path=ID.jpg".into(),
            file_id: "ID".into(),
            file_name: "ID.jpg".into(),
            file_size: 1234,
        };
        // Byte for byte what kotlinx writes for the app's `ServerContentInfo`
        assert_eq!(
            image_manifest("Poster", &slide, "2026-01-01 00:00:00"),
            concat!(
                r#"{"id":"ID","program_id":"com.samsung.android.ePaper","deploy_type":"MOBILE","#,
                r#""version":1,"name":"Poster","content_type":"ImageContent","schedule":[{"#,
                r#""start_date":"1970-01-01","stop_date":"2999-12-31","start_time":"00:00:00","#,
                r#""contents":[{"file_id":"ID","file_size":"1234","file_name":"ID.jpg","#,
                r#""file_path":"/home/owner/content/Downloads/vxtplayer/epaper/mobile/contents/ID/ID.jpg","#,
                r#""image_url":"https://1.2.3.4:6869/image?path=ID.jpg","duration":86400000}]}],"#,
                r#""create_time":"2026-01-01 00:00:00"}"#
            )
        );
    }

    #[test]
    fn content_ids_look_like_the_apps() {
        let id = new_content_id();
        let uuid = id.strip_prefix("content_info_").unwrap();
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid, uuid.to_lowercase());
        assert_ne!(id, new_content_id());
    }

    #[test]
    fn playlist_lists_every_slide_in_order() {
        let slide = |n: usize| Slide {
            image_url: format!("http://h/image/{n}"),
            file_id: format!("ID{n}"),
            file_name: format!("ID{n}.jpg"),
            file_size: 100 + n,
        };
        let interval = SlideInterval::parse("1:30").unwrap();
        let s = playlist_manifest(
            "PL",
            "Lobby",
            &[slide(0), slide(1), slide(2)],
            interval,
            "t",
        );
        assert!(s.starts_with(r#"{"id":"PL","program_id":"com.samsung.android.ePaper""#));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["content_type"], "PlaylistContent");
        assert_eq!(v["id"], "PL");
        let contents = v["schedule"][0]["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        for (n, c) in contents.iter().enumerate() {
            assert_eq!(c["file_id"], format!("ID{n}"));
            assert_eq!(c["duration"], 90 * 60_000);
            assert_eq!(c["file_size"], (100 + n).to_string());
            assert_eq!(
                c["file_path"],
                format!("/home/owner/content/Downloads/vxtplayer/epaper/mobile/contents/ID{n}/ID{n}.jpg")
            );
        }
    }

    #[test]
    fn slide_interval_limits() {
        assert_eq!(SlideInterval::parse("30").unwrap().minutes(), 30);
        assert_eq!(SlideInterval::parse("2h").unwrap().minutes(), 120);
        assert_eq!(SlideInterval::parse("1h30").unwrap().minutes(), 90);
        assert_eq!(SlideInterval::parse("24:00").unwrap().minutes(), 1440);
        assert_eq!(SlideInterval::parse("5").unwrap().millis(), 300_000);
        assert!(SlideInterval::parse("4").is_err());
        assert!(SlideInterval::parse("24:01").is_err());
        assert!(SlideInterval::parse("1:60").is_err());
        assert!(SlideInterval::parse("soon").is_err());
        assert_eq!(
            SlideInterval::from_minutes(90).unwrap().to_string(),
            "1 h 30 min"
        );
        assert_eq!(SlideInterval::from_minutes(240).unwrap().to_string(), "4 h");
        assert_eq!(
            SlideInterval::from_minutes(45).unwrap().to_string(),
            "45 minutes"
        );
    }

    #[test]
    fn start_time_parsing() {
        assert_eq!(StartTime::parse("8:00").unwrap().minutes(), 480);
        assert_eq!(StartTime::parse("08:05").unwrap().manifest(), "08:05:00");
        assert_eq!(StartTime::parse("8h30").unwrap().to_string(), "08:30");
        assert_eq!(StartTime::parse("23h").unwrap().minutes(), 23 * 60);
        assert_eq!(StartTime::parse("0:00").unwrap().minutes(), 0);
        for bad in [
            "24:00", "8:60", "8", "8:", ":30", "noon", "-1:00", "8:5x", "008:00",
        ] {
            assert!(StartTime::parse(bad).is_err(), "{bad}");
        }
        assert!(StartTime::from_minutes(1439).is_ok());
        assert!(StartTime::from_minutes(1440).is_err());
    }

    #[test]
    fn schedule_has_one_block_per_start_time_in_time_order() {
        let slide = |id: &str| Slide {
            image_url: format!("http://h/image/{id}"),
            file_id: id.into(),
            file_name: format!("{id}.jpg"),
            file_size: 7,
        };
        let blocks = [
            ScheduleBlock {
                start: StartTime::parse("12:00").unwrap(),
                slide: slide("B"),
            },
            ScheduleBlock {
                start: StartTime::parse("8:00").unwrap(),
                slide: slide("A"),
            },
        ];
        let s = schedule_manifest("SC", "Day", &blocks, "t");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["content_type"], "ScheduleContent");
        let schedule = v["schedule"].as_array().unwrap();
        assert_eq!(schedule.len(), 2);
        assert_eq!(schedule[0]["start_time"], "08:00:00");
        assert_eq!(schedule[0]["start_date"], "1970-01-01");
        assert_eq!(schedule[0]["stop_date"], "2999-12-31");
        assert_eq!(schedule[1]["start_time"], "12:00:00");
        for (block, id) in schedule.iter().zip(["A", "B"]) {
            let contents = block["contents"].as_array().unwrap();
            assert_eq!(contents.len(), 1);
            assert_eq!(contents[0]["duration"], 86_400_000);
            assert_eq!(
                contents[0]["file_path"],
                format!(
                    "/home/owner/content/Downloads/vxtplayer/epaper/mobile/contents/{id}/{id}.jpg"
                )
            );
        }
    }

    #[test]
    fn create_time_format() {
        let t = create_time_now();
        assert_eq!(t.len(), 19);
        assert_eq!(&t[10..11], " ");
    }
}
