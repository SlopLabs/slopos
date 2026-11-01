//! Users: the people who use this computer, and each one's account type,
//! password and automatic login. Every change is `/bin/users`, which privd
//! runs once someone allows it — the person's own password by its owner's
//! word, anything else by an administrator's — and passwords travel only
//! on the mechanism's standard input.

use slopos_accounts_core::listing::Listed;
use slopos_appkit::{
    Button, ButtonRow, Dialog, LineEditInput, List, ListRow, Node, Notice, PropertyList, Section,
    Text, Tone, column, row,
};
use slopos_theme_core::{TextRole, space};

use crate::field::{Field, Left, caption_text, with_caption, with_error};
use crate::{Ask, Asked, Effect as AppEffect, Input, Msg as AppMsg, Panel, Person, Purpose};

pub const LIST: &str = "org.slopos.accounts.list";
pub const MANAGE: &str = "org.slopos.accounts.manage";
pub const OWN_PASSWORD: &str = "org.slopos.accounts.password";

/// The first person, who keeps the workspace at `/src` and so stays.
const FIRST_PERSON: u32 = slopos_abi::fs::SESSION_UID;

#[derive(Clone, Debug)]
pub enum Msg {
    /// The people, and whether the list says what holds for each: from the
    /// accounts record through privd, else from the user database alone.
    Listed(Vec<Listed>, bool),
    Choose(usize),
    Admin,
    Autologin,
    Add,
    Password,
    Rename,
    Remove,
    Cancel,
    Confirm,
    RemoveFiles,
    DialogAdmin,
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
        name: String,
        own: bool,
        current: Field,
        password: Field,
        again: Field,
    },
    Rename {
        name: String,
        new: Field,
    },
    Remove {
        name: String,
        files: bool,
    },
}

pub struct Users {
    me: Person,
    people: Option<Vec<Listed>>,
    /// Whether the list says who has a password and who logs in by
    /// themselves.
    whole: bool,
    chosen: Option<usize>,
    dialog: Option<Form>,
    /// Why the dialog's last request did not do what it asked, shown in it
    /// so nothing typed has to be typed again.
    dialog_trouble: Option<String>,
    asking: Option<Request>,
    said: Option<(Tone, String, Option<String>)>,
}

fn kind(person: &Listed) -> &'static str {
    match person.admin {
        true => "Administrator",
        false => "Standard user",
    }
}

/// A password as `users` reads a new one: typed twice.
fn twice(input: &mut Input, password: &Field) {
    input.line(password.text());
    input.line(password.text());
}

impl Users {
    pub fn new(me: Person) -> Users {
        Users {
            me,
            people: None,
            whole: false,
            chosen: None,
            dialog: None,
            dialog_trouble: None,
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

    fn person(&self) -> Option<&Listed> {
        self.people.as_ref()?.get(self.chosen?)
    }

    fn me_listed(&self) -> Option<&Listed> {
        self.people.as_ref()?.iter().find(|p| p.uid == self.me.uid)
    }

    fn ask(
        &mut self,
        request: Request,
        action: &'static str,
        args: Vec<String>,
        input: Input,
    ) -> Vec<AppEffect> {
        self.asking = Some(request);
        self.said = None;
        vec![AppEffect::Ask(Ask {
            purpose: Purpose::Users(request),
            action,
            args,
            input,
        })]
    }

    fn manage(&mut self, request: Request, args: &[&str], input: Input) -> Vec<AppEffect> {
        let args = args.iter().map(|a| a.to_string()).collect();
        self.ask(request, MANAGE, args, input)
    }

    /// Why the dialog cannot be answered yet, in a sentence, `None` when it
    /// can.
    fn not_yet(&self) -> Option<&'static str> {
        let taken = |name: &str| self.people.iter().flatten().any(|p| p.name == name);
        match self.dialog.as_ref()? {
            Form::Add {
                name,
                password,
                again,
                own,
                own_again,
                ..
            } => {
                if !slopos_accounts_core::valid_name(name.text()) {
                    return Some(NAME_RULE);
                }
                if taken(name.text()) {
                    return Some("Someone has that name already.");
                }
                if password.text() != again.text() {
                    return Some("The two passwords differ.");
                }
                if self.needs_own_password() && own.text().is_empty() {
                    return Some("Choose a password for yourself first.");
                }
                if own.text() != own_again.text() {
                    return Some("Your two passwords differ.");
                }
                None
            }
            Form::Password {
                own,
                current,
                password,
                again,
                ..
            } => {
                let has = self.whole && self.me_listed().is_some_and(|me| me.password);
                if *own && has && current.text().is_empty() {
                    return Some("Type your password as it is now.");
                }
                if password.text().is_empty() {
                    return Some("Type the new password.");
                }
                (password.text() != again.text()).then_some("The two passwords differ.")
            }
            Form::Rename { name, new } => {
                if new.text() == name {
                    return Some("Type a new name.");
                }
                if !slopos_accounts_core::valid_name(new.text()) {
                    return Some(NAME_RULE);
                }
                taken(new.text()).then_some("Someone has that name already.")
            }
            Form::Remove { .. } => None,
        }
    }

    /// Whether `users` asks for the person's password now before a new
    /// one: when the record says they have one, or, read from the database
    /// alone, when they typed one.
    fn asks_current(&self, current: &Field) -> bool {
        match self.whole {
            true => self.me_listed().is_some_and(|me| me.password),
            false => !current.text().is_empty(),
        }
    }

    /// An administrator with no password who adds someone sets one first,
    /// which `users` asks for before the new person's.
    fn needs_own_password(&self) -> bool {
        self.whole && self.me_listed().is_some_and(|me| me.admin && !me.password)
    }

    fn confirm(&mut self) -> Vec<AppEffect> {
        if self.not_yet().is_some() || self.asking.is_some() {
            return Vec::new();
        }
        let Some(mut dialog) = self.dialog.take() else {
            return Vec::new();
        };
        self.dialog_trouble = None;
        let effects = match &mut dialog {
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
                let mut input = Input::new();
                if self.needs_own_password() {
                    args.push("--adder-password");
                    twice(&mut input, own);
                }
                if !password.text().is_empty() {
                    args.push("--password");
                    twice(&mut input, password);
                }
                let args: Vec<String> = args.into_iter().map(String::from).collect();
                self.ask(Request::Add, MANAGE, args, input)
            }
            Form::Password {
                name,
                own,
                current,
                password,
                ..
            } => {
                let mut input = Input::new();
                match *own {
                    true => {
                        if self.asks_current(current) {
                            input.line(current.text());
                        }
                        twice(&mut input, password);
                        let args = vec![String::from("passwd")];
                        self.ask(Request::Password, OWN_PASSWORD, args, input)
                    }
                    false => {
                        twice(&mut input, password);
                        let name = name.clone();
                        self.manage(Request::Password, &["passwd", &name], input)
                    }
                }
            }
            Form::Rename { name, new } => {
                let (old, new) = (name.clone(), new.text().to_string());
                self.manage(Request::Rename, &["rename", &old, &new], Input::default())
            }
            Form::Remove { name, files } => {
                let name = name.clone();
                let mut args = vec!["remove", name.as_str()];
                if *files {
                    args.push("--home");
                }
                self.manage(Request::Remove, &args, Input::default())
            }
        };
        self.dialog = Some(dialog);
        effects
    }

    fn field(&mut self, which: Msg, input: LineEditInput) -> Vec<AppEffect> {
        self.dialog_trouble = None;
        let field = match (&mut self.dialog, which) {
            (Some(Form::Add { name, .. }), Msg::Name(_)) => name,
            (Some(Form::Add { password, .. } | Form::Password { password, .. }), Msg::New(_)) => {
                password
            }
            (Some(Form::Add { again, .. } | Form::Password { again, .. }), Msg::Again(_)) => again,
            (Some(Form::Add { own, .. }), Msg::Own(_)) => own,
            (Some(Form::Add { own_again, .. }), Msg::OwnAgain(_)) => own_again,
            (Some(Form::Password { current, .. }), Msg::Current(_)) => current,
            (Some(Form::Rename { new, .. }), Msg::Name(_)) => new,
            _ => return Vec::new(),
        };
        match field.edit(input) {
            Left::Enter => self.confirm(),
            Left::Nothing => Vec::new(),
        }
    }

    pub fn update(&mut self, msg: Msg) -> Vec<AppEffect> {
        match msg {
            Msg::Listed(people, whole) => {
                if let Some(me) = people.iter().find(|p| p.uid == self.me.uid) {
                    self.me.admin = me.admin;
                }
                let chosen = self.person().map(|p| p.uid).unwrap_or(self.me.uid);
                self.chosen = people.iter().position(|p| p.uid == chosen).or(Some(0));
                self.people = Some(people);
                self.whole = whole;
            }
            Msg::Choose(row) => self.chosen = Some(row),
            Msg::Admin => {
                let Some(person) = self.person() else {
                    return Vec::new();
                };
                let (verb, name) = match person.admin {
                    true => ("demote", person.name.clone()),
                    false => ("promote", person.name.clone()),
                };
                return self.manage(Request::Admin, &[verb, &name], Input::default());
            }
            Msg::Autologin => {
                let Some(person) = self.person() else {
                    return Vec::new();
                };
                let name = match person.autologin {
                    true => String::from("--off"),
                    false => person.name.clone(),
                };
                return self.manage(Request::Autologin, &["autologin", &name], Input::default());
            }
            Msg::Add => {
                self.dialog = Some(Form::Add {
                    name: Field::default(),
                    admin: false,
                    password: Field::for_secret(),
                    again: Field::for_secret(),
                    own: Field::for_secret(),
                    own_again: Field::for_secret(),
                });
                return vec![AppEffect::Users(Effect::Read)];
            }
            Msg::Password => {
                if let Some(person) = self.person() {
                    self.dialog = Some(Form::Password {
                        name: person.name.clone(),
                        own: person.uid == self.me.uid,
                        current: Field::for_secret(),
                        password: Field::for_secret(),
                        again: Field::for_secret(),
                    });
                    return vec![AppEffect::Users(Effect::Read)];
                }
            }
            Msg::Rename => {
                if let Some(person) = self.person() {
                    self.dialog = Some(Form::Rename {
                        name: person.name.clone(),
                        new: Field::with(person.name.clone()),
                    });
                }
            }
            Msg::Remove => {
                if let Some(person) = self.person() {
                    self.dialog = Some(Form::Remove {
                        name: person.name.clone(),
                        files: false,
                    });
                }
            }
            Msg::Cancel => {
                if let Some(mut dialog) = self.dialog.take() {
                    wipe(&mut dialog);
                }
                self.dialog_trouble = None;
            }
            Msg::Confirm => return self.confirm(),
            Msg::RemoveFiles => {
                if let Some(Form::Remove { files, .. }) = &mut self.dialog {
                    *files = !*files;
                }
            }
            Msg::DialogAdmin => {
                if let Some(Form::Add { admin, .. }) = &mut self.dialog {
                    *admin = !*admin;
                }
            }
            which @ (Msg::Name(input)
            | Msg::Current(input)
            | Msg::New(input)
            | Msg::Again(input)
            | Msg::Own(input)
            | Msg::OwnAgain(input)) => return self.field(which, input),
        }
        Vec::new()
    }

    pub fn answered(&mut self, request: Request, asked: Asked) -> Vec<AppEffect> {
        self.asking = None;
        let trouble = asked.trouble();
        match (&trouble, self.dialog.as_mut()) {
            (None, Some(dialog)) => {
                wipe(dialog);
                self.dialog = None;
            }
            (Some(why), Some(dialog)) => {
                if let Form::Password { current, .. } = dialog {
                    current.wipe();
                }
                self.dialog_trouble = Some(why.clone());
                return vec![AppEffect::Users(Effect::Read)];
            }
            (_, None) => {}
        }
        self.said = Some(match trouble {
            None => {
                let notes = match &asked {
                    Asked::Ran { notes, .. } => more(notes),
                    Asked::Refused { .. } => None,
                };
                match notes {
                    Some(notes) => (Tone::Info, String::from(done(request, true)), Some(notes)),
                    None => (Tone::Success, String::from(done(request, false)), None),
                }
            }
            Some(why) => (Tone::Danger, why, None),
        });
        vec![AppEffect::Users(Effect::Read)]
    }

    pub fn view(&self) -> Node<AppMsg> {
        let about = "The people who use this computer.";
        let Some(people) = &self.people else {
            let reading = Notice::new(Tone::Info, "Reading who uses this computer");
            return crate::page(Panel::Users, about, vec![reading.into()], None);
        };
        let mut parts: Vec<Node<AppMsg>> = Vec::new();
        if let Some((tone, said, body)) = &self.said {
            let notice = Notice::new(*tone, said.clone());
            parts.push(match body {
                Some(body) => notice.body(body.clone()).into(),
                None => notice.into(),
            });
        }
        if !self.me.admin {
            parts.push(
                Notice::new(
                    Tone::Info,
                    "Changing anything but your own password takes an administrator's password",
                )
                .into(),
            );
        }
        let rows = people
            .iter()
            .map(|p| {
                let you = match p.uid == self.me.uid {
                    true => " · You",
                    false => "",
                };
                ListRow::new(p.name.clone())
                    .avatar()
                    .detail(format!("{}{you}", kind(p)))
            })
            .collect();
        parts.push(
            Section::new(
                "People",
                column(
                    space::SM,
                    vec![
                        List::new(rows)
                            .selected(self.chosen)
                            .on_select(|row| AppMsg::Users(Msg::Choose(row)))
                            .into(),
                        row(
                            0,
                            vec![
                                Button::new("Add user…")
                                    .on_press(AppMsg::Users(Msg::Add))
                                    .enabled(self.asking.is_none() && self.whole)
                                    .into(),
                            ],
                        ),
                    ],
                ),
            )
            .into(),
        );
        if !self.whole {
            parts.push(caption_text(
                "Adding someone waits until who has a password can be read.",
                Tone::Secondary,
            ));
        }
        if let Some(person) = self.person() {
            parts.push(self.person_section(person));
        }
        crate::page(Panel::Users, about, parts, None)
    }

    fn person_section(&self, person: &Listed) -> Node<AppMsg> {
        let idle = self.asking.is_none();
        let told = self.whole && !person.withheld;
        let mut facts = PropertyList::new().row("Account type", kind(person));
        if told {
            facts = facts.row("Password", if person.password { "Set" } else { "None" });
        }
        facts = facts.row("Home folder", person.home.clone());
        let mut parts: Vec<Node<AppMsg>> = vec![facts.into()];
        let admins = self.people.iter().flatten().filter(|p| p.admin).count();
        let kept_admin = match person.admin {
            true if admins == 1 => Some("The computer's last administrator stays one."),
            true if person.uid == self.me.uid => {
                Some("Another administrator can make you a standard user.")
            }
            _ => None,
        };
        parts.push(Node::Checkbox {
            checked: person.admin,
            label: String::from("Administrator"),
            on_toggle: Some(AppMsg::Users(Msg::Admin)),
            enabled: idle && kept_admin.is_none(),
        });
        if let Some(why) = kept_admin {
            parts.push(caption_text(why, Tone::Secondary));
        }
        if told {
            parts.push(Node::Checkbox {
                checked: person.autologin,
                label: String::from("Log in automatically when the computer starts"),
                on_toggle: Some(AppMsg::Users(Msg::Autologin)),
                enabled: idle,
            });
            if person.autologin {
                parts.push(
                    Text::new("Anyone who starts the computer can use this account.")
                        .role(TextRole::Caption)
                        .tone(Tone::Secondary)
                        .wrap()
                        .into(),
                );
            }
        }
        let password = match person.uid == self.me.uid {
            true => "Change password…",
            false => "Set password…",
        };
        let removable = person.uid != self.me.uid && person.uid != FIRST_PERSON;
        parts.push(row(
            space::SM,
            vec![
                Button::new(password)
                    .on_press(AppMsg::Users(Msg::Password))
                    .enabled(idle)
                    .into(),
                Button::new("Rename…")
                    .on_press(AppMsg::Users(Msg::Rename))
                    .enabled(idle)
                    .into(),
                Button::new("Remove…")
                    .destructive()
                    .on_press(AppMsg::Users(Msg::Remove))
                    .enabled(idle && removable)
                    .into(),
            ],
        ));
        if !removable {
            let why = match person.uid == self.me.uid {
                true => "You can't remove yourself.",
                false => "The computer's first user stays.",
            };
            parts.push(
                Text::new(why)
                    .role(TextRole::Caption)
                    .tone(Tone::Secondary)
                    .wrap()
                    .into(),
            );
        }
        Section::new(person.name.clone(), column(space::MD, parts)).into()
    }

    pub fn dialog(&self) -> Option<Node<AppMsg>> {
        let dialog = self.dialog.as_ref()?;
        let error = self.not_yet();
        let footer = |verb: &str, destructive: bool| {
            let cancel = Button::new("Cancel").on_press(AppMsg::Users(Msg::Cancel));
            let go = Button::new(verb)
                .on_press(AppMsg::Users(Msg::Confirm))
                .enabled(error.is_none() && self.asking.is_none());
            match destructive {
                true => ButtonRow::new().cancel(cancel).destructive(go),
                false => ButtonRow::new().cancel(cancel).confirm(go),
            }
        };
        let (title, content, footer) = match dialog {
            Form::Add {
                name,
                admin,
                password,
                again,
                own,
                own_again,
            } => {
                let unsafe_without = password.text().is_empty().then_some((
                    "With no password, anyone at the computer can log in as them.",
                    Tone::Secondary,
                ));
                let mut form = PropertyList::new()
                    .row_with(
                        "Name",
                        with_caption(
                            name.line("Such as grace", |i| AppMsg::Users(Msg::Name(i))),
                            Some((NAME_RULE, Tone::Secondary)),
                        ),
                    )
                    .row_with(
                        "Password",
                        password.secret("Password", |i| AppMsg::Users(Msg::New(i))),
                    )
                    .row_with(
                        "Again",
                        with_caption(
                            again.secret("The password again", |i| AppMsg::Users(Msg::Again(i))),
                            unsafe_without,
                        ),
                    );
                if self.needs_own_password() {
                    form = form
                        .row_with(
                            "Your password",
                            with_caption(
                                own.secret("Your new password", |i| AppMsg::Users(Msg::Own(i))),
                                Some((
                                    "You have none, which an administrator of a computer others use needs.",
                                    Tone::Secondary,
                                )),
                            ),
                        )
                        .row_with(
                            "Yours again",
                            own_again.secret("Your new password again", |i| {
                                AppMsg::Users(Msg::OwnAgain(i))
                            }),
                        );
                }
                let mut parts: Vec<Node<AppMsg>> = vec![
                    form.into(),
                    Node::Checkbox {
                        checked: *admin,
                        label: String::from("Administrator"),
                        on_toggle: Some(AppMsg::Users(Msg::DialogAdmin)),
                        enabled: true,
                    },
                ];
                if self.people.iter().flatten().any(|p| p.autologin) {
                    parts.push(caption_text(
                        "Nobody logs in automatically once they are added.",
                        Tone::Secondary,
                    ));
                }
                ("Add a user", parts, footer("Add", false))
            }
            Form::Password {
                name,
                own,
                current,
                password,
                again,
            } => {
                let mut form = PropertyList::new();
                let has = !self.whole || self.me_listed().is_some_and(|me| me.password);
                if *own && has {
                    form = form.row_with(
                        "Current",
                        current.secret("Your password now", |i| AppMsg::Users(Msg::Current(i))),
                    );
                }
                form = form
                    .row_with(
                        "New",
                        password.secret("The new password", |i| AppMsg::Users(Msg::New(i))),
                    )
                    .row_with(
                        "Again",
                        again.secret("The new password again", |i| AppMsg::Users(Msg::Again(i))),
                    );
                let (title, verb) = match own {
                    true => (String::from("Change your password"), "Change"),
                    false => (format!("Set {name}'s password"), "Set"),
                };
                let parts = vec![form.into()];
                return Some(self.dialog_node(title, parts, error, footer(verb, false)));
            }
            Form::Rename { new, .. } => {
                let field = new.line("A new name", |i| AppMsg::Users(Msg::Name(i)));
                let parts = vec![
                    PropertyList::new()
                        .row_with(
                            "Name",
                            with_caption(field, Some((NAME_RULE, Tone::Secondary))),
                        )
                        .into(),
                ];
                ("Rename", parts, footer("Rename", false))
            }
            Form::Remove { name, files } => {
                let parts = vec![
                    Text::new(format!("They can no longer log in as {name}."))
                        .wrap()
                        .into(),
                    Node::Checkbox {
                        checked: *files,
                        label: String::from("Delete their files too"),
                        on_toggle: Some(AppMsg::Users(Msg::RemoveFiles)),
                        enabled: true,
                    },
                ];
                let title = format!("Remove {name}?");
                return Some(self.dialog_node(title, parts, error, footer("Remove", true)));
            }
        };
        Some(self.dialog_node(title.to_string(), content, error, footer))
    }

    fn dialog_node(
        &self,
        title: String,
        mut parts: Vec<Node<AppMsg>>,
        error: Option<&str>,
        footer: ButtonRow<AppMsg>,
    ) -> Node<AppMsg> {
        let typed = match &self.dialog {
            Some(Form::Add { name, .. }) => !name.text().is_empty(),
            Some(Form::Password { password, .. }) => !password.text().is_empty(),
            Some(Form::Rename { name, new }) => new.text() != name,
            _ => false,
        };
        match &self.dialog_trouble {
            Some(trouble) => parts.push(with_error(Node::Empty, Some(trouble))),
            None if typed => parts.push(with_error(Node::Empty, error)),
            None => {}
        }
        Dialog::new(title)
            .content(column(space::MD, parts))
            .footer(footer)
            .into()
    }
}

const NAME_RULE: &str =
    "A name is a lowercase letter, then lowercase letters, digits, hyphens and underscores.";

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

fn done(request: Request, noted: bool) -> &'static str {
    match request {
        Request::Add if noted => "The user is added.",
        Request::Add => "The new user can log in now.",
        Request::Password => "The password is changed.",
        Request::Rename => "The name is changed.",
        Request::Remove => "The user is removed.",
        Request::Admin => "The account type is changed.",
        Request::Autologin => "Who logs in automatically is changed.",
    }
}

fn wipe(dialog: &mut Form) {
    match dialog {
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
