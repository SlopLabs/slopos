//! Users: the people who use this computer, and each one's account type,
//! password and automatic login. Every change is `/bin/users`, which privd
//! runs once someone allows it — the person's own password by its owner's
//! word, anything else by an administrator's — and passwords travel only
//! on the mechanism's standard input.

use slopos_accounts_core::listing::Listed;
use slopos_appkit::{
    ActionRow, Button, ButtonRow, CrossAxisAlignment, Dialog, Group, LineEditInput, List, ListRow,
    Node, Notice, Page, PropertyList, Section, Switch, Text, Tone, column, row,
};
use slopos_theme_core::{Surface, TextRole, space};

use crate::field::{Field, Left, caption_text, locked_text, with_caption};
use crate::{Ask, Asked, Effect as AppEffect, Input, Msg as AppMsg, Panel, Person, Purpose};

pub const LIST: &str = "org.slopos.accounts.list";
pub const MANAGE: &str = "org.slopos.accounts.manage";
pub const OWN_PASSWORD: &str = "org.slopos.accounts.password";

/// The first person, who keeps the workspace at `/src` and so stays.
const FIRST_PERSON: u32 = slopos_abi::fs::SESSION_UID;

const ABOUT: &str = "The people who use this computer.";

#[derive(Clone, Debug)]
pub enum Msg {
    /// The people, and whether the list says what holds for each: from the
    /// accounts record through privd, else from the user database alone.
    Listed(Vec<Listed>, bool),
    /// By row among everyone but the person running Settings.
    Open(usize),
    Back,
    Admin,
    Autologin,
    Add,
    Password,
    Rename,
    Remove,
    Cancel,
    Confirm,
    /// Row 0 a standard user, row 1 an administrator.
    Kind(usize),
    /// Row 0 keeps the removed person's files, row 1 deletes them.
    Files(usize),
    Name(LineEditInput),
    Current(LineEditInput),
    New(LineEditInput),
    Again(LineEditInput),
    Own(LineEditInput),
    OwnAgain(LineEditInput),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// List the people; answer [`Msg::Listed`].
    Read,
}

/// Which change an answer is to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Add,
    Password,
    Rename,
    Remove,
    Admin,
    Autologin,
}

impl Request {
    fn failed(self) -> &'static str {
        match self {
            Request::Add => "The user couldn't be added",
            Request::Password => "The password couldn't be changed",
            Request::Rename => "The name couldn't be changed",
            Request::Remove => "The user couldn't be removed",
            Request::Admin => "The account type couldn't be changed",
            Request::Autologin => "Automatic login couldn't be changed",
        }
    }
}

enum Form {
    Add {
        name: Field,
        admin: bool,
        password: Field,
        again: Field,
        /// The asker's own new password, when they administer the computer
        /// with none, which a computer shared needs.
        own: Field,
        own_again: Field,
    },
    Password {
        uid: u32,
        name: String,
        current: Field,
        password: Field,
        again: Field,
    },
    Rename {
        uid: u32,
        name: String,
        new: Field,
    },
    Remove {
        name: String,
        home: String,
        files: bool,
    },
}

impl Form {
    fn request(&self) -> Request {
        match self {
            Form::Add { .. } => Request::Add,
            Form::Password { .. } => Request::Password,
            Form::Rename { .. } => Request::Rename,
            Form::Remove { .. } => Request::Remove,
        }
    }

    fn done(&self, noted: bool) -> &'static str {
        match self {
            Form::Add { .. } if noted => "The user is added.",
            Form::Add { .. } => "The new user can log in now.",
            Form::Password { .. } => "The password is changed.",
            Form::Rename { .. } => "The name is changed.",
            Form::Remove { .. } => "The user is removed.",
        }
    }

    fn about(&self) -> About {
        match self {
            Form::Add { .. } | Form::Remove { .. } => About::People,
            Form::Password { uid, .. } | Form::Rename { uid, .. } => About::Person(*uid),
        }
    }

    fn wipe(&mut self) {
        match self {
            Form::Add {
                password,
                again,
                own,
                own_again,
                ..
            } => {
                for field in [password, again, own, own_again] {
                    field.wipe();
                }
            }
            Form::Password {
                current,
                password,
                again,
                ..
            } => {
                for field in [current, password, again] {
                    field.wipe();
                }
            }
            Form::Rename { .. } | Form::Remove { .. } => {}
        }
    }
}

struct Editing {
    form: Form,
    tried: bool,
    trouble: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Name,
    Current,
    New,
    Again,
    Own,
    OwnAgain,
}

struct Asking {
    request: Request,
    /// The uid whose switch it turned, and to what.
    turned: Option<(u32, bool)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum About {
    Person(u32),
    People,
}

struct Said {
    tone: Tone,
    title: String,
    body: Option<String>,
    about: About,
}

pub struct Users {
    me: Person,
    people: Option<Vec<Listed>>,
    /// Whether the list says who has a password and who logs in by
    /// themselves.
    whole: bool,
    /// The uid of the other person whose page shows.
    open: Option<u32>,
    editing: Option<Editing>,
    asking: Option<Asking>,
    said: Option<Said>,
}

fn kind(person: &Listed) -> &'static str {
    match person.admin {
        true => KINDS[1].0,
        false => KINDS[0].0,
    }
}

/// A password as `users` reads a new one: typed twice.
fn twice(input: &mut Input, password: &Field) {
    input.line(password.text());
    input.line(password.text());
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

impl Users {
    pub fn new(me: Person) -> Users {
        Users {
            me,
            people: None,
            whole: false,
            open: None,
            editing: None,
            asking: None,
            said: None,
        }
    }

    pub fn whole(&self) -> bool {
        self.whole
    }

    pub fn me_admin(&self) -> bool {
        self.me.admin
    }

    pub fn show_everyone(&mut self) {
        self.open = None;
        self.said = None;
    }

    fn listed(&self, uid: u32) -> Option<&Listed> {
        self.people.as_ref()?.iter().find(|p| p.uid == uid)
    }

    fn me_listed(&self) -> Option<&Listed> {
        self.listed(self.me.uid)
    }

    fn shown(&self) -> Option<&Listed> {
        self.listed(self.open.unwrap_or(self.me.uid))
    }

    fn others(&self) -> impl Iterator<Item = &Listed> {
        let me = self.me.uid;
        self.people.iter().flatten().filter(move |p| p.uid != me)
    }

    fn told(&self, person: &Listed) -> bool {
        self.whole && !person.withheld
    }

    fn ask(
        &mut self,
        asking: Asking,
        action: &'static str,
        args: Vec<String>,
        input: Input,
    ) -> Vec<AppEffect> {
        let purpose = Purpose::Users(asking.request);
        self.asking = Some(asking);
        self.said = None;
        vec![AppEffect::Ask(Ask {
            purpose,
            action,
            args,
            input,
        })]
    }

    fn kept_admin(&self, person: &Listed) -> Option<&'static str> {
        let admins = self.people.iter().flatten().filter(|p| p.admin).count();
        match person.admin {
            true if admins == 1 => Some("The computer's last administrator stays one."),
            true if person.uid == self.me.uid => {
                Some("Another administrator can make you a standard user.")
            }
            _ => None,
        }
    }

    fn turned(&self, request: Request, person: &Listed, now: bool) -> bool {
        match &self.asking {
            Some(Asking {
                request: asked,
                turned: Some((uid, on)),
            }) if *asked == request && *uid == person.uid => *on,
            _ => now,
        }
    }

    fn turn(&mut self, request: Request) -> Vec<AppEffect> {
        if self.asking.is_some() {
            return Vec::new();
        }
        let Some(person) = self.shown() else {
            return Vec::new();
        };
        let (uid, name) = (person.uid, person.name.clone());
        let (on, args) = match request {
            Request::Admin => {
                if self.kept_admin(person).is_some() {
                    return Vec::new();
                }
                let verb = match person.admin {
                    true => "demote",
                    false => "promote",
                };
                (!person.admin, vec![String::from(verb), name])
            }
            _ => {
                let whom = match person.autologin {
                    true => String::from("--off"),
                    false => name,
                };
                (!person.autologin, vec![String::from("autologin"), whom])
            }
        };
        let asking = Asking {
            request,
            turned: Some((uid, on)),
        };
        self.ask(asking, MANAGE, args, Input::default())
    }

    /// So a switch never flicks back before the people are read again.
    fn keep_turned(&mut self, request: Request, uid: u32, on: bool) {
        for person in self.people.iter_mut().flatten() {
            match request {
                Request::Admin if person.uid == uid => person.admin = on,
                Request::Autologin => person.autologin = on && person.uid == uid,
                _ => {}
            }
        }
        if request == Request::Admin && uid == self.me.uid {
            self.me.admin = on;
        }
    }

    fn wrong(&self) -> Vec<(Slot, &'static str)> {
        let Some(editing) = &self.editing else {
            return Vec::new();
        };
        let taken = |name: &str| self.people.iter().flatten().any(|p| p.name == name);
        let named = |name: &str| match slopos_accounts_core::valid_name(name) {
            false => Some(NAME_RULE),
            true => taken(name).then_some("Someone has that name already."),
        };
        let mut wrong = Vec::new();
        let mut hold = |slot: Slot, why: Option<&'static str>| {
            wrong.extend(why.map(|why| (slot, why)));
        };
        match &editing.form {
            Form::Add {
                name,
                password,
                again,
                own,
                own_again,
                ..
            } => {
                hold(Slot::Name, named(name.text()));
                hold(
                    Slot::Again,
                    (password.text() != again.text()).then_some(DIFFER),
                );
                let unset = self.needs_own_password() && own.text().is_empty();
                hold(
                    Slot::Own,
                    unset.then_some("Choose a password for yourself first."),
                );
                hold(
                    Slot::OwnAgain,
                    (own.text() != own_again.text()).then_some(DIFFER),
                );
            }
            Form::Password {
                uid,
                current,
                password,
                again,
                ..
            } => {
                let asked = *uid == self.me.uid
                    && self.whole
                    && self.me_listed().is_some_and(|me| me.password);
                hold(
                    Slot::Current,
                    (asked && current.text().is_empty())
                        .then_some("Type your password as it is now."),
                );
                hold(
                    Slot::New,
                    password
                        .text()
                        .is_empty()
                        .then_some("Type the new password."),
                );
                hold(
                    Slot::Again,
                    (password.text() != again.text()).then_some(DIFFER),
                );
            }
            Form::Rename { name, new, .. } => hold(
                Slot::Name,
                match new.text() == name {
                    true => Some("Type a new name."),
                    false => named(new.text()),
                },
            ),
            Form::Remove { .. } => {}
        }
        wrong
    }

    /// An administrator with no password who adds someone sets one first,
    /// which `users` asks for before the new person's.
    fn needs_own_password(&self) -> bool {
        self.whole && self.me_listed().is_some_and(|me| me.admin && !me.password)
    }

    fn confirm(&mut self) -> Vec<AppEffect> {
        if self.asking.is_some() {
            return Vec::new();
        }
        let wrong = !self.wrong().is_empty();
        let needs_own = self.needs_own_password();
        let me = self.me.uid;
        // `users` asks for the password as it is when the record says there
        // is one, or, with the database alone read, when one was typed.
        let has = self
            .whole
            .then(|| self.me_listed().is_some_and(|me| me.password));
        let Some(editing) = &mut self.editing else {
            return Vec::new();
        };
        editing.trouble = None;
        if wrong {
            editing.tried = true;
            return Vec::new();
        }
        let mut input = Input::new();
        let (action, args) = match &editing.form {
            Form::Add {
                name,
                admin,
                password,
                own,
                ..
            } => {
                let mut args = vec!["add", name.text()];
                if *admin {
                    args.push("--admin");
                }
                if needs_own {
                    args.push("--adder-password");
                    twice(&mut input, own);
                }
                if !password.text().is_empty() {
                    args.push("--password");
                    twice(&mut input, password);
                }
                (MANAGE, owned(&args))
            }
            Form::Password {
                uid,
                current,
                password,
                ..
            } if *uid == me => {
                if has.unwrap_or(!current.text().is_empty()) {
                    input.line(current.text());
                }
                twice(&mut input, password);
                (OWN_PASSWORD, owned(&["passwd"]))
            }
            Form::Password { name, password, .. } => {
                twice(&mut input, password);
                (MANAGE, owned(&["passwd", name]))
            }
            Form::Rename { name, new, .. } => (MANAGE, owned(&["rename", name, new.text()])),
            Form::Remove { name, files, .. } => {
                let mut args = vec!["remove", name.as_str()];
                if *files {
                    args.push("--home");
                }
                (MANAGE, owned(&args))
            }
        };
        let asking = Asking {
            request: editing.form.request(),
            turned: None,
        };
        self.ask(asking, action, args, input)
    }

    fn typed(&mut self, which: Msg) -> Vec<AppEffect> {
        let Some(editing) = &mut self.editing else {
            return Vec::new();
        };
        editing.trouble = None;
        let (field, input) = match (&mut editing.form, which) {
            (Form::Add { name, .. }, Msg::Name(input)) => (name, input),
            (Form::Add { password, .. } | Form::Password { password, .. }, Msg::New(input)) => {
                (password, input)
            }
            (Form::Add { again, .. } | Form::Password { again, .. }, Msg::Again(input)) => {
                (again, input)
            }
            (Form::Add { own, .. }, Msg::Own(input)) => (own, input),
            (Form::Add { own_again, .. }, Msg::OwnAgain(input)) => (own_again, input),
            (Form::Password { current, .. }, Msg::Current(input)) => (current, input),
            (Form::Rename { new, .. }, Msg::Name(input)) => (new, input),
            _ => return Vec::new(),
        };
        match field.edit(input) {
            Left::Enter => self.confirm(),
            Left::Escape => self.update(Msg::Cancel),
            Left::Nothing => Vec::new(),
        }
    }

    /// Reads the people again, so the form is held to who is here now.
    fn edit(&mut self, form: Form) -> Vec<AppEffect> {
        self.editing = Some(Editing {
            form,
            tried: false,
            trouble: None,
        });
        vec![AppEffect::Users(Effect::Read)]
    }

    fn removable(&self, person: &Listed) -> bool {
        person.uid != self.me.uid && person.uid != FIRST_PERSON
    }

    pub fn update(&mut self, msg: Msg) -> Vec<AppEffect> {
        let idle = self.asking.is_none() && self.editing.is_none();
        match msg {
            Msg::Listed(people, whole) => {
                if let Some(me) = people.iter().find(|p| p.uid == self.me.uid) {
                    self.me.admin = me.admin;
                }
                if self
                    .open
                    .is_some_and(|uid| !people.iter().any(|p| p.uid == uid))
                {
                    self.open = None;
                }
                self.people = Some(people);
                self.whole = whole;
            }
            Msg::Open(row) => {
                let opened = self.others().nth(row).map(|p| p.uid);
                if opened.is_some() {
                    self.open = opened;
                    self.said = None;
                }
            }
            Msg::Back => self.show_everyone(),
            Msg::Admin => return self.turn(Request::Admin),
            Msg::Autologin => return self.turn(Request::Autologin),
            Msg::Add if idle && self.whole => {
                return self.edit(Form::Add {
                    name: Field::default(),
                    admin: false,
                    password: Field::for_secret(),
                    again: Field::for_secret(),
                    own: Field::for_secret(),
                    own_again: Field::for_secret(),
                });
            }
            Msg::Password if idle => {
                if let Some(person) = self.shown() {
                    return self.edit(Form::Password {
                        uid: person.uid,
                        name: person.name.clone(),
                        current: Field::for_secret(),
                        password: Field::for_secret(),
                        again: Field::for_secret(),
                    });
                }
            }
            Msg::Rename if idle => {
                if let Some(person) = self.shown() {
                    return self.edit(Form::Rename {
                        uid: person.uid,
                        name: person.name.clone(),
                        new: Field::with(person.name.clone()),
                    });
                }
            }
            Msg::Remove if idle => {
                if let Some(person) = self.shown().filter(|p| self.removable(p)) {
                    return self.edit(Form::Remove {
                        name: person.name.clone(),
                        home: person.home.clone(),
                        files: false,
                    });
                }
            }
            Msg::Add | Msg::Password | Msg::Rename | Msg::Remove => {}
            Msg::Cancel => {
                if self.asking.is_none()
                    && let Some(mut editing) = self.editing.take()
                {
                    editing.form.wipe();
                }
            }
            Msg::Confirm => return self.confirm(),
            Msg::Kind(row) => {
                if let Some(Form::Add { admin, .. }) = self.editing.as_mut().map(|e| &mut e.form) {
                    *admin = row == 1;
                }
            }
            Msg::Files(row) => {
                if let Some(Form::Remove { files, .. }) = self.editing.as_mut().map(|e| &mut e.form)
                {
                    *files = row == 1;
                }
            }
            which @ (Msg::Name(_)
            | Msg::Current(_)
            | Msg::New(_)
            | Msg::Again(_)
            | Msg::Own(_)
            | Msg::OwnAgain(_)) => return self.typed(which),
        }
        Vec::new()
    }

    /// A person's own no says nothing; any other is said where it was asked.
    pub fn answered(&mut self, request: Request, asked: Asked) -> Vec<AppEffect> {
        let read = vec![AppEffect::Users(Effect::Read)];
        let turned = self.asking.take().and_then(|asking| asking.turned);
        let declined = matches!(
            asked,
            Asked::Refused {
                by_person: true,
                ..
            }
        );
        let trouble = asked.trouble();
        if let Some((uid, on)) = turned {
            match trouble {
                None => self.keep_turned(request, uid, on),
                Some(_) if declined => {}
                Some(why) => {
                    self.said = Some(Said {
                        tone: Tone::Danger,
                        title: String::from(request.failed()),
                        body: Some(why),
                        about: About::Person(uid),
                    });
                }
            }
            return read;
        }
        let Some(editing) = &mut self.editing else {
            return read;
        };
        if let Some(why) = trouble {
            if let Form::Password { current, .. } = &mut editing.form {
                current.wipe();
            }
            editing.trouble = (!declined).then_some(why);
            return read;
        }
        let notes = match &asked {
            Asked::Ran { notes, .. } => more(notes),
            Asked::Refused { .. } => None,
        };
        let tone = match notes {
            Some(_) => Tone::Info,
            None => Tone::Success,
        };
        self.said = Some(Said {
            tone,
            title: String::from(editing.form.done(notes.is_some())),
            body: notes,
            about: editing.form.about(),
        });
        if request == Request::Remove {
            self.open = None;
        }
        editing.form.wipe();
        self.editing = None;
        read
    }

    pub fn view(&self) -> Node<AppMsg> {
        if self.people.is_none() {
            let reading = Notice::new(Tone::Info, "Reading who uses this computer");
            return crate::page(Panel::Users, ABOUT, vec![reading.into()], None);
        }
        match self.open.and_then(|uid| self.listed(uid)) {
            Some(person) => self.person_page(person),
            None => self.everyone(),
        }
    }

    fn said_of(&self, here: impl Fn(About) -> bool) -> Option<Node<AppMsg>> {
        let said = self.said.as_ref().filter(|said| here(said.about))?;
        let notice = Notice::new(said.tone, said.title.clone());
        Some(match &said.body {
            Some(body) => notice.body(body.clone()).into(),
            None => notice.into(),
        })
    }

    fn everyone(&self) -> Node<AppMsg> {
        let mine = |about: About| about == About::Person(self.me.uid);
        let mut parts: Vec<Node<AppMsg>> = Vec::new();
        if let Some(me) = self.me_listed() {
            let mut yours: Vec<Node<AppMsg>> = vec![self.account(me).into()];
            yours.extend(self.said_of(mine));
            if !self.me.admin {
                yours.push(locked_text(
                    "Anything but your own password asks for an administrator's password.",
                ));
            }
            parts.push(Section::new("Your account", column(space::SM, yours)).into());
        }
        let rows: Vec<ListRow> = self
            .others()
            .map(|p| {
                ListRow::new(p.name.clone())
                    .avatar()
                    .detail(self.summary(p))
                    .opens()
            })
            .collect();
        // Said over the list, which is in view wherever *Add user…* was.
        let mut theirs: Vec<Node<AppMsg>> =
            self.said_of(|about| !mine(about)).into_iter().collect();
        theirs.push(match rows.is_empty() {
            true => Text::new("Nobody else uses this computer.")
                .tone(Tone::Secondary)
                .wrap()
                .into(),
            false => Node::Card {
                surface: Surface::Raised,
                shadow: false,
                child: Box::new(
                    List::new(rows)
                        .on_activate(|row| AppMsg::Users(Msg::Open(row)))
                        .into(),
                ),
            },
        });
        theirs.push(row(
            0,
            vec![
                Button::new("Add user…")
                    .on_press(AppMsg::Users(Msg::Add))
                    .enabled(self.asking.is_none() && self.whole)
                    .into(),
            ],
        ));
        if !self.whole {
            theirs.push(caption_text(
                "Adding someone waits until who has a password can be read.",
                Tone::Secondary,
            ));
        }
        let heading = match self.me_listed() {
            Some(_) => "Other users",
            None => "Users",
        };
        parts.push(Section::new(heading, column(space::SM, theirs)).into());
        crate::page(Panel::Users, ABOUT, parts, None)
    }

    fn summary(&self, person: &Listed) -> String {
        let mut said = String::from(kind(person));
        if self.told(person) {
            if !person.password {
                said.push_str(" · No password");
            }
            if person.autologin {
                said.push_str(" · Logs in automatically");
            }
        }
        said
    }

    fn person_page(&self, person: &Listed) -> Node<AppMsg> {
        let mut theirs: Vec<Node<AppMsg>> = vec![self.account(person).into()];
        theirs.extend(self.said_of(|_| true));
        if !self.me.admin {
            theirs.push(locked_text("Changes ask for an administrator's password."));
        }
        let leaving: Node<AppMsg> = match self.removable(person) {
            false => caption_text(
                "The computer's first user can't be removed.",
                Tone::Secondary,
            ),
            true => Group::new()
                .row(
                    ActionRow::new(format!("Remove {}", person.name))
                        .detail("They can no longer log in. You choose whether their files go too.")
                        .trailing(
                            Button::new("Remove…")
                                .destructive()
                                .on_press(AppMsg::Users(Msg::Remove))
                                .enabled(self.asking.is_none()),
                        ),
                )
                .into(),
        };
        Page::new(person.name.clone())
            .back("Back to Users", AppMsg::Users(Msg::Back))
            .content(column(space::XL, vec![column(space::SM, theirs), leaving]))
            .into()
    }

    fn account(&self, person: &Listed) -> Group<AppMsg> {
        let idle = self.asking.is_none();
        let mine = person.uid == self.me.uid;
        let button = |label: &str, msg: Msg| {
            Button::new(label)
                .on_press(AppMsg::Users(msg))
                .enabled(idle)
        };
        let face = Node::VStack {
            children: vec![
                Node::Avatar {
                    name: person.name.clone(),
                },
                Text::new(person.name.clone()).role(TextRole::Title).into(),
                Text::new(kind(person)).tone(Tone::Secondary).into(),
            ],
            spacing: space::XS,
            align: CrossAxisAlignment::Center,
        };
        let name = ActionRow::new("Name")
            .detail(format!("Renaming keeps the home folder, {}.", person.home))
            .trailing(button("Rename…", Msg::Rename));
        let password = ActionRow::new("Password");
        let password = match (self.told(person), person.password, mine) {
            (false, ..) => password.trailing(button("Change…", Msg::Password)),
            (true, true, true) => password
                .detail("You log in with a password.")
                .trailing(button("Change…", Msg::Password)),
            (true, true, false) => password
                .detail("They log in with a password.")
                .trailing(button("Change…", Msg::Password)),
            (true, false, true) => password
                .detail("You have none, so anyone at this computer can log in as you.")
                .trailing(button("Set…", Msg::Password)),
            (true, false, false) => password
                .detail("They have none, so anyone at this computer can log in as them.")
                .trailing(button("Set…", Msg::Password)),
        };
        let mut group = Group::new().row(face).row(name).row(password);
        if self.told(person) {
            group = group.row(
                Switch::new(
                    "Log in automatically",
                    self.turned(Request::Autologin, person, person.autologin),
                )
                .detail(
                    "When the computer starts, it opens this account without asking for a password.",
                )
                .on_toggle(AppMsg::Users(Msg::Autologin))
                .enabled(idle),
            );
        }
        let kept = self.kept_admin(person);
        group.row(
            Switch::new(
                "Administrator",
                self.turned(Request::Admin, person, person.admin),
            )
            .detail(kept.unwrap_or(KINDS[1].1))
            .on_toggle(AppMsg::Users(Msg::Admin))
            .enabled(idle && kept.is_none()),
        )
    }

    pub fn dialog(&self) -> Option<Node<AppMsg>> {
        let editing = self.editing.as_ref()?;
        let idle = self.asking.is_none();
        let wrong = self.wrong();
        let why = |slot: Slot, left: bool| {
            let found = wrong.iter().find(|(held, _)| *held == slot);
            found.map(|(_, why)| *why).filter(|_| editing.tried || left)
        };
        let captioned = |field: Node<AppMsg>, error: Option<&str>, hint: Option<&str>| {
            let danger = error.map(|said| (said, Tone::Danger));
            with_caption(field, danger.or(hint.map(|said| (said, Tone::Secondary))))
        };
        let line = |field: &Field,
                    error: Option<&str>,
                    placeholder: &str,
                    on_input: fn(LineEditInput) -> AppMsg| {
            let node = field.checked(placeholder, error.is_some(), on_input);
            captioned(node, error, Some(NAME_RULE))
        };
        let secret = |field: &Field,
                      error: Option<&str>,
                      placeholder: &str,
                      hint: Option<&str>,
                      on_input: fn(LineEditInput) -> AppMsg| {
            let node = field.secret(placeholder, error.is_some(), on_input);
            captioned(node, error, hint)
        };
        let footer = |verb: &str, destructive: bool| {
            let cancel = Button::new("Cancel")
                .on_press(AppMsg::Users(Msg::Cancel))
                .enabled(idle);
            let go = Button::new(verb)
                .on_press(AppMsg::Users(Msg::Confirm))
                .enabled(idle);
            match destructive {
                true => ButtonRow::new().cancel(cancel).destructive(go),
                false => ButtonRow::new().cancel(cancel).confirm(go),
            }
        };
        let (title, mut parts, footer) = match &editing.form {
            Form::Add {
                name,
                admin,
                password,
                again,
                own,
                own_again,
            } => {
                let unset = match password.text().is_empty() {
                    true => "Without one, anyone at this computer can log in as them.",
                    false => "They can change it once they log in.",
                };
                let form = PropertyList::new()
                    .row_with(
                        "Name",
                        line(name, why(Slot::Name, name.left), "Such as grace", |i| {
                            AppMsg::Users(Msg::Name(i))
                        }),
                    )
                    .row_with(
                        "Password",
                        secret(password, None, "Leave empty for none", None, |i| {
                            AppMsg::Users(Msg::New(i))
                        }),
                    )
                    .row_with(
                        "Confirm",
                        secret(
                            again,
                            why(Slot::Again, again.left),
                            "The same password again",
                            Some(unset),
                            |i| AppMsg::Users(Msg::Again(i)),
                        ),
                    );
                let kinds = KINDS
                    .iter()
                    .map(|(title, detail)| ListRow::new(*title).detail(*detail))
                    .collect();
                let mut parts: Vec<Node<AppMsg>> = vec![
                    form.into(),
                    List::new(kinds)
                        .selected(Some(usize::from(*admin)))
                        .on_select(|row| AppMsg::Users(Msg::Kind(row)))
                        .into(),
                ];
                if self.needs_own_password() {
                    parts.push(
                        Notice::new(Tone::Info, "Choose a password for yourself too")
                            .body(
                                "You administer this computer with none, which a computer others use needs.",
                            )
                            .into(),
                    );
                    parts.push(
                        PropertyList::new()
                            .row_with(
                                "Password",
                                secret(
                                    own,
                                    why(Slot::Own, own.left),
                                    "Your new password",
                                    None,
                                    |i| AppMsg::Users(Msg::Own(i)),
                                ),
                            )
                            .row_with(
                                "Confirm",
                                secret(
                                    own_again,
                                    why(Slot::OwnAgain, own_again.left),
                                    "The same password again",
                                    None,
                                    |i| AppMsg::Users(Msg::OwnAgain(i)),
                                ),
                            )
                            .into(),
                    );
                }
                if self.people.iter().flatten().any(|p| p.autologin) {
                    parts.push(caption_text(
                        "Nobody logs in automatically once they are added.",
                        Tone::Secondary,
                    ));
                }
                (String::from("Add a user"), parts, footer("Add", false))
            }
            Form::Password {
                uid,
                name,
                current,
                password,
                again,
            } => {
                let mine = *uid == self.me.uid;
                let has = !self.whole || self.me_listed().is_some_and(|me| me.password);
                let mut form = PropertyList::new();
                if mine && has {
                    form = form.row_with(
                        "Current",
                        secret(
                            current,
                            why(Slot::Current, current.left),
                            "Your password now",
                            None,
                            |i| AppMsg::Users(Msg::Current(i)),
                        ),
                    );
                }
                form = form
                    .row_with(
                        "New",
                        secret(
                            password,
                            why(Slot::New, password.left),
                            "The new password",
                            None,
                            |i| AppMsg::Users(Msg::New(i)),
                        ),
                    )
                    .row_with(
                        "Confirm",
                        secret(
                            again,
                            why(Slot::Again, again.left),
                            "The same password again",
                            None,
                            |i| AppMsg::Users(Msg::Again(i)),
                        ),
                    );
                let lacks = self
                    .listed(*uid)
                    .is_some_and(|p| self.told(p) && !p.password);
                let verb = match lacks {
                    true => "Set",
                    false => "Change",
                };
                let title = match mine {
                    true => format!("{verb} your password"),
                    false => format!("{verb} {name}'s password"),
                };
                (title, vec![form.into()], footer(verb, false))
            }
            Form::Rename { name, new, .. } => {
                let changed = new.left && new.text() != name;
                let form = PropertyList::new().row_with(
                    "Name",
                    line(new, why(Slot::Name, changed), "A new name", |i| {
                        AppMsg::Users(Msg::Name(i))
                    }),
                );
                (
                    format!("Rename {name}"),
                    vec![form.into()],
                    footer("Rename", false),
                )
            }
            Form::Remove { name, home, files } => {
                let choices = vec![
                    ListRow::new("Keep their files").detail(format!(
                        "Their home folder, {home}, stays on this computer."
                    )),
                    ListRow::new("Delete their files")
                        .detail(format!("Everything in {home} is deleted for good.")),
                ];
                let parts = vec![
                    Text::new(format!("They can no longer log in as {name}."))
                        .wrap()
                        .into(),
                    List::new(choices)
                        .selected(Some(usize::from(*files)))
                        .on_select(|row| AppMsg::Users(Msg::Files(row)))
                        .into(),
                ];
                (format!("Remove {name}?"), parts, footer("Remove", true))
            }
        };
        if let Some(trouble) = &editing.trouble {
            parts.push(
                Notice::new(Tone::Danger, editing.form.request().failed())
                    .body(trouble.clone())
                    .into(),
            );
        }
        Some(
            Dialog::new(title)
                .content(column(space::MD, parts))
                .footer(footer)
                .into(),
        )
    }
}

const NAME_RULE: &str =
    "A name is a lowercase letter, then lowercase letters, digits, hyphens and underscores.";

const DIFFER: &str = "The two passwords differ.";

const KINDS: [(&str, &str); 2] = [
    ("Standard user", "Uses apps and changes their own settings."),
    (
        "Administrator",
        "Adds and removes users, installs software and changes settings for the whole computer.",
    ),
];

/// What `users` noted beside the change it made, as sentences: the clauses
/// after the first of its last line.
fn more(notes: &str) -> Option<String> {
    let last = notes.lines().rev().find(|line| !line.trim().is_empty())?;
    let clauses: Vec<String> = last
        .split("; ")
        .skip(1)
        .map(|clause| {
            let mut chars = clause.trim().chars();
            let first = chars
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_default();
            format!("{first}{}.", chars.as_str().trim_end_matches('.'))
        })
        .collect();
    (!clauses.is_empty()).then(|| clauses.join(" "))
}

#[cfg(test)]
pub(crate) fn sample() -> Vec<Listed> {
    vec![
        Listed {
            name: "user".into(),
            uid: 1000,
            home: "/home/user".into(),
            admin: true,
            password: false,
            autologin: true,
            remote: true,
            withheld: false,
        },
        Listed {
            name: "ada".into(),
            uid: 1001,
            home: "/home/ada".into(),
            admin: false,
            password: true,
            autologin: false,
            remote: false,
            withheld: false,
        },
    ]
}
