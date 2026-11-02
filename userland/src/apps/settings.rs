//! `/bin/settings` — the Settings app: the panels of
//! [`slopos_settings_app`] in a window, and what they ask of the system
//! carried out on threads of their own, each answered as a message. Every
//! change goes through the tool that makes it at a terminal — `netctl`,
//! `users`, `keymap` through privd, or the person's own preferences — so
//! the app can change nothing a terminal cannot.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use slopos_abi::PAGE_SIZE;
use slopos_abi::syscall::types::{UserCpuInfo, UserSysInfo, UserUtsname};
use slopos_accounts_core::listing::{self, Listed};
use slopos_appkit::{Action, App, AppSender, ImageData, Node, SIDEBAR_WIDTH, WindowSpec};
use slopos_keymap_core::board::Board;
use slopos_settings_app::{
    Ask, Asked, Effect, Msg, Panel, Person, Settings, about, desktop, keyboard, network, users,
};
use slopos_settings_core::prefs::Prefs;

use crate::syscall::core as sys_core;

const WIDTH: i32 = 920;
const HEIGHT: i32 = 660;
const PAGE_LEAST_WIDTH: i32 = 440;
const LEAST_HEIGHT: i32 = 480;

/// Where the system's pictures are, and below a home where a person's own
/// go, as `~/.local/share/backgrounds` is for GNOME.
const SYSTEM_PICTURES: &str = "/usr/share/slopos/wallpapers";
const OWN_PICTURES: &str = ".local/share/backgrounds";

pub struct SettingsWindow {
    app: Settings,
    sender: Option<AppSender<Msg>>,
    lanes: [Option<mpsc::Sender<Effect>>; Lane::COUNT],
}

/// Effects that touch one thing run one after another in the order asked,
/// so the last of a quick run of changes is the one that stays; effects that
/// touch different things run beside each other, so a question privd waits
/// on holds up no read.
#[derive(Clone, Copy)]
enum Lane {
    Privd,
    People,
    Prefs,
    Pictures,
    Network,
    Facts,
}

impl Lane {
    const COUNT: usize = 6;

    fn of(effect: &Effect) -> Lane {
        match effect {
            Effect::Ask(_) => Lane::Privd,
            Effect::Users(_) => Lane::People,
            Effect::Desktop(desktop::Effect::Thumbnails(_)) => Lane::Pictures,
            Effect::Keyboard(_) | Effect::Desktop(_) => Lane::Prefs,
            Effect::Network(_) => Lane::Network,
            Effect::About(_) => Lane::Facts,
        }
    }
}

impl SettingsWindow {
    pub fn new(panel: Panel) -> SettingsWindow {
        SettingsWindow {
            app: Settings::new(panel, person()),
            sender: None,
            lanes: Default::default(),
        }
    }

    fn perform(&mut self, effects: Vec<Effect>) {
        let Some(sender) = self.sender.clone() else {
            return;
        };
        for effect in effects {
            let lane = &mut self.lanes[Lane::of(&effect) as usize];
            let effect = match lane {
                Some(queue) => match queue.send(effect) {
                    Ok(()) => continue,
                    Err(mpsc::SendError(effect)) => effect,
                },
                None => effect,
            };
            *lane = worker(sender.clone());
            match lane.as_ref() {
                Some(queue) => {
                    let _ = queue.send(effect);
                }
                None => {
                    let _ = writeln!(std::io::stderr(), "settings: no thread for what was asked");
                    carry_out(effect, &sender);
                }
            }
        }
    }
}

fn worker(sender: AppSender<Msg>) -> Option<mpsc::Sender<Effect>> {
    let (queue, effects) = mpsc::channel::<Effect>();
    std::thread::Builder::new()
        .name("settings".into())
        .spawn(move || {
            for effect in effects {
                carry_out(effect, &sender);
            }
        })
        .ok()?;
    Some(queue)
}

/// Who runs the window, as the user database says.
fn person() -> Person {
    let uid = crate::syscall::process::getuid();
    let name = crate::accounts::name_of(uid).unwrap_or_default();
    let admin = uid == 0 || crate::accounts::admins().contains(&name);
    Person { uid, name, admin }
}

fn carry_out(effect: Effect, sender: &AppSender<Msg>) {
    let msg = match effect {
        Effect::Ask(ask) => ask_privd(ask),
        Effect::Network(network::Effect::Read) => Msg::Network(network::Msg::Read(
            crate::network_settings::read()
                .and_then(|kept| crate::network_settings::live().map(|live| (kept, live))),
        )),
        Effect::Users(users::Effect::Read) => people(),
        Effect::Keyboard(keyboard::Effect::Read) => Msg::Keyboard(keyboard::Msg::Read(layouts())),
        Effect::Keyboard(keyboard::Effect::Keep(name)) => {
            Msg::Keyboard(keyboard::Msg::Kept(keep_layout(name)))
        }
        Effect::Desktop(desktop::Effect::Read) => {
            sender.send(Msg::Desktop(desktop::Msg::Prefs(crate::prefs::read_own())));
            Msg::Desktop(desktop::Msg::Pictures(pictures()))
        }
        Effect::Desktop(desktop::Effect::Thumbnails(paths)) => {
            for path in paths {
                if let Some(image) = thumbnail(&path) {
                    sender.send(Msg::Desktop(desktop::Msg::Thumbnail(path, image)));
                }
            }
            return;
        }
        Effect::Desktop(desktop::Effect::Save(changes)) => {
            Msg::Desktop(desktop::Msg::Saved(save_prefs(&changes)))
        }
        Effect::Desktop(desktop::Effect::Add(path)) => {
            Msg::Desktop(desktop::Msg::Added(add_picture(&path)))
        }
        Effect::Desktop(desktop::Effect::Remove(path)) => {
            remove_picture(&path);
            return;
        }
        Effect::About(about::Effect::Read) => Msg::About(about::Msg::Read(facts())),
    };
    sender.send(msg);
}

fn ask_privd(ask: Ask) -> Msg {
    let args: Vec<&str> = ask.args.iter().map(String::as_str).collect();
    let ran = crate::privd::run_capturing(ask.action, &args, &ask.input);
    let asked = match ran {
        Ok(ran) => Asked::Ran {
            status: ran.status,
            said: ran.err,
            notes: ran.out,
        },
        Err(not) => Asked::Refused {
            why: match not.from_privd {
                true => not.reason,
                false => String::from("The system couldn't be asked. Try again in a moment."),
            },
            by_person: not.by_person,
        },
    };
    Msg::Answered(ask.purpose, asked)
}

/// The people, as the accounts record says through privd, or as the user
/// database alone does, which says nothing of passwords.
fn people() -> Msg {
    let listed = crate::privd::run_capturing(users::LIST, &["list", "--porcelain"], b"")
        .ok()
        .filter(|ran| ran.status == 0)
        .and_then(|ran| listing::parse(&ran.out));
    if let Some(people) = listed {
        return Msg::Users(users::Msg::Listed(people, true));
    }
    let admins = crate::accounts::admins();
    let people = crate::accounts::people()
        .into_iter()
        .map(|p| Listed {
            admin: admins.iter().any(|a| *a == p.name),
            name: p.name,
            uid: p.uid,
            home: p.home,
            password: false,
            autologin: false,
            remote: false,
            withheld: true,
        })
        .collect();
    Msg::Users(users::Msg::Listed(people, false))
}

fn layouts() -> keyboard::Read {
    keyboard::Read {
        layouts: crate::keymap::layouts()
            .into_iter()
            .map(|l| keyboard::Layout {
                board: crate::keymap::table(&l.name)
                    .ok()
                    .map(|table| Board::of(&table)),
                name: l.name,
                title: l.title,
            })
            .collect(),
        yours: crate::prefs::read_own()
            .keyboard_layout()
            .map(str::to_string),
        computer: crate::keymap::computer_layout()
            .unwrap_or_else(|| crate::keymap::BUILT_IN.to_string()),
    }
}

/// Make `changes` to the person's preferences as their file holds them now,
/// so a change made elsewhere meanwhile stays: what the file then holds, or
/// why not, as a sentence.
fn save_prefs(changes: &[desktop::Change]) -> Result<Prefs, String> {
    let path = crate::prefs::own_path().ok_or("You have no home to keep preferences in.")?;
    let mut prefs = crate::prefs::read(&path);
    for (key, value) in changes {
        match value {
            Some(value) => prefs.set(*key, value).map_err(|why| why.to_string())?,
            None => prefs.reset(*key),
        }
    }
    crate::prefs::write(&path, &prefs)
        .map_err(|_| String::from("Your preferences couldn't be kept."))?;
    Ok(prefs)
}

/// Keep `name` as the person's layout and have init load it.
fn keep_layout(name: Option<String>) -> Result<(), keyboard::Kept> {
    use crate::keymap::Own;
    crate::keymap::keep_own(name.as_deref()).map_err(|why| {
        let not = |said: &str| keyboard::Kept::Not(String::from(said));
        match why {
            Own::NoSuchLayout => not("That layout isn't installed."),
            Own::NoHome => not("You have no home to keep a layout in."),
            Own::NotKept(_) => not("Your layout couldn't be kept."),
            Own::NotLoaded(_) => keyboard::Kept::Later,
        }
    })
}

fn own_pictures() -> Option<PathBuf> {
    crate::prefs::own_home().map(|home| Path::new(&home).join(OWN_PICTURES))
}

fn pictures() -> Vec<desktop::Picture> {
    let found = |dir: &Path, own: bool| -> Vec<desktop::Picture> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut pictures: Vec<desktop::Picture> = entries
            .flatten()
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("png"))
            })
            .filter_map(|e| {
                let file = e.file_name().into_string().ok()?;
                Some(desktop::Picture {
                    path: e.path().to_string_lossy().into_owned(),
                    name: desktop::picture_name(&file),
                    own,
                })
            })
            .collect();
        pictures.sort_by(|a, b| a.name.cmp(&b.name));
        pictures
    };
    let mut all = found(Path::new(SYSTEM_PICTURES), false);
    if let Some(own) = own_pictures() {
        all.extend(found(&own, true));
    }
    all
}

/// The picture at `path` the size a gallery shows it, covering its box.
fn thumbnail(path: &str) -> Option<ImageData> {
    use slopos_appkit::widgets::gallery::{THUMBNAIL_HEIGHT, THUMBNAIL_WIDTH};
    let image = crate::regular_file::picture(path)?;
    let bitmap = slopos_gfx::image::BitmapRef::new(image.width, image.height, &image.pixels)?;
    let (w, h) = (THUMBNAIL_WIDTH as u32, THUMBNAIL_HEIGHT as u32);
    let mut pixels = vec![Default::default(); (w * h) as usize];
    slopos_gfx::image::cover_into(bitmap, w, h, &mut pixels).then_some(())?;
    ImageData::new(w, h, pixels)
}

/// Copy the picture at `from` among the person's own, under its own name or
/// one beside it: the path kept, or why not, as a sentence.
fn add_picture(from: &str) -> Result<String, String> {
    let from = match from.strip_prefix("~/") {
        Some(rest) => crate::prefs::own_home()
            .map(|home| format!("{home}/{rest}"))
            .ok_or("You have no home to keep pictures in.")?,
        None => from.to_string(),
    };
    let real =
        std::fs::canonicalize(&from).map_err(|_| format!("There is no picture at {from}."))?;
    let real = real.to_string_lossy();
    let bytes = crate::regular_file::read(&real, crate::regular_file::PICTURE_MAX, None)
        .ok_or_else(|| format!("There is no picture at {from}."))?;
    if slopos_image::decode(&bytes, slopos_image::DecodeOptions::default()).is_err() {
        return Err(String::from(
            "That isn't a PNG picture this computer can show.",
        ));
    }
    let dir = own_pictures().ok_or("You have no home to keep pictures in.")?;
    std::fs::create_dir_all(&dir).map_err(|_| String::from("Your pictures couldn't be kept."))?;
    let file = Path::new(&from)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("picture.png");
    let (stem, ext) = file.rsplit_once('.').unwrap_or((file, "png"));
    let mut to = dir.join(file);
    let mut n = 2;
    let same = |to: &Path| {
        crate::regular_file::read(&to.to_string_lossy(), bytes.len() + 1, None).as_deref()
            == Some(&bytes[..])
    };
    while to.exists() && !same(&to) {
        to = dir.join(format!("{stem}-{n}.{ext}"));
        n += 1;
    }
    std::fs::write(&to, &bytes).map_err(|_| String::from("The picture couldn't be copied."))?;
    Ok(to.to_string_lossy().into_owned())
}

/// Take away a picture of the person's own, and nothing else.
fn remove_picture(path: &str) {
    let path = Path::new(path);
    let own = own_pictures().is_some_and(|dir| path.parent() == Some(dir.as_path()));
    if own {
        let _ = std::fs::remove_file(path);
    }
}

fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn facts() -> about::Facts {
    let mut uts = UserUtsname::new();
    sys_core::uname(&mut uts);
    let mut sys = UserSysInfo::default();
    sys_core::sys_info(&mut sys);
    let mut cpu = UserCpuInfo::default();
    sys_core::cpu_info(&mut cpu);
    let tag = std::fs::read_to_string("/usr/share/slopos/build-tag").unwrap_or_default();
    let version = match tag.trim() {
        "" => format!("{} {}", text(&uts.sysname), text(&uts.release)),
        tag => format!("{} {} ({tag})", text(&uts.sysname), text(&uts.release)),
    };
    let storage = crate::syscall::fs::statfs_path(c"/".as_ptr())
        .ok()
        .map(|s| {
            let block = s.f_bsize as u64;
            (s.f_blocks * block, s.f_bavail * block)
        });
    about::Facts {
        version,
        processor: text(&cpu.brand_string),
        processors: cpu.cpu_count.max(1),
        memory: u64::from(sys.total_pages).saturating_mul(PAGE_SIZE),
        storage,
        name: text(&uts.nodename),
    }
}

impl App for SettingsWindow {
    type Message = Msg;

    fn view(&self) -> Node<Msg> {
        self.app.view()
    }

    fn update(&mut self, msg: Msg) -> Action {
        let (action, effects) = self.app.update(msg);
        self.perform(effects);
        action
    }

    fn on_start(&mut self, sender: AppSender<Msg>) -> Action {
        self.sender = Some(sender);
        self.perform(self.app.start());
        Action::None
    }

    fn tick_interval_ms(&self) -> Option<u64> {
        self.app.tick_interval_ms()
    }

    fn tick(&mut self) -> Action {
        let effects = self.app.tick();
        self.perform(effects);
        Action::None
    }

    fn title(&self) -> String {
        slopos_settings_app::TITLE.into()
    }

    fn app_id(&self) -> &str {
        slopos_settings_app::APP_ID
    }

    fn window(&self) -> WindowSpec {
        WindowSpec::new(WIDTH, HEIGHT).min_size(SIDEBAR_WIDTH + PAGE_LEAST_WIDTH, LEAST_HEIGHT)
    }
}

pub fn settings_main() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ids: Vec<&str> = Panel::ALL.iter().map(|p| p.id()).collect();
    let panel = match args.as_slice() {
        [] => Some(Panel::Network),
        [flag, id] if flag == "--page" => Panel::named(id),
        _ => None,
    };
    let Some(panel) = panel else {
        eprintln!("usage: settings [--page {}]", ids.join("|"));
        sys_core::exit_with_code(2)
    };
    slopos_appkit::run_app(SettingsWindow::new(panel))
}
