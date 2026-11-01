//! Desktop: your background and your clock, preferences of yours that your
//! desktop takes as you change them. A picture added here is copied into
//! `~/.local/share/backgrounds`, so it stays when the one it came from goes.

use slopos_appkit::{
    Button, ButtonRow, Dialog, Gallery, GalleryTile, ImageData, LineEditInput, List, ListRow, Node,
    Notice, Section, Text, Tone, column, row,
};
use slopos_settings_core::background::Fit;
use slopos_settings_core::prefs::{Key, Prefs};
use slopos_theme_core::{GRADIENTS, space};

use crate::field::{Field, Left, with_error};
use crate::{Effect as AppEffect, Msg as AppMsg, Panel};

/// A picture the desktop may show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub path: String,
    pub name: String,
    /// The person added it, so may take it away.
    pub own: bool,
}

#[derive(Clone, Debug)]
pub enum Msg {
    Prefs(Prefs),
    Pictures(Vec<Picture>),
    Thumbnail(String, ImageData),
    Choose(usize),
    Fit(usize),
    Clock,
    Add,
    Path(LineEditInput),
    Cancel,
    Confirm,
    /// A picture added, or why it was not.
    Added(Result<String, String>),
    Remove,
    ConfirmRemove,
    /// The preferences as kept once a change went in, or why it did not.
    Saved(Result<Prefs, String>),
}

/// A preference to set, or to take back to its default for `None`.
pub type Change = (Key, Option<String>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Read the preferences and the pictures; answer [`Msg::Prefs`] and
    /// [`Msg::Pictures`].
    Read,
    /// Make thumbnails of the pictures at these paths; answer a
    /// [`Msg::Thumbnail`] for each that decodes.
    Thumbnails(Vec<String>),
    /// Make these changes to the preferences as the file holds them now;
    /// answer [`Msg::Saved`].
    Save(Vec<Change>),
    /// Copy the picture at this path among the person's own; answer
    /// [`Msg::Added`].
    Add(String),
    /// Take away a picture the person added.
    Remove(String),
}

#[derive(Default)]
pub struct Desktop {
    prefs: Option<Prefs>,
    /// Saves asked for whose answer has not come, which an answer before
    /// the last would show the person undone.
    saving: usize,
    pictures: Vec<Picture>,
    thumbnails: Vec<(String, ImageData)>,
    /// Pictures whose thumbnail was asked for, made or not.
    asked: Vec<String>,
    adding: Option<Field>,
    removing: Option<Picture>,
    said: Option<(Tone, String)>,
}

impl Desktop {
    fn picture(&self) -> Option<&str> {
        self.prefs.as_ref()?.background().picture
    }

    /// Every tile: the gradients, then the pictures, the one chosen among
    /// them even when it is in neither place pictures are found.
    fn tiles(&self) -> Vec<(GalleryTile, Choice)> {
        static SHADED: std::sync::OnceLock<Vec<ImageData>> = std::sync::OnceLock::new();
        let shaded = SHADED.get_or_init(|| GRADIENTS.iter().map(ImageData::thumbnail_of).collect());
        let mut tiles: Vec<(GalleryTile, Choice)> = GRADIENTS
            .iter()
            .zip(shaded)
            .map(|(g, image)| {
                let tile = GalleryTile::new(g.name, Some(image.clone()));
                (tile, Choice::Gradient(g.id))
            })
            .collect();
        let mut pictures = self.pictures.clone();
        if let Some(chosen) = self.picture()
            && !pictures.iter().any(|p| p.path == chosen)
        {
            let name = chosen.rsplit('/').next().unwrap_or(chosen);
            pictures.push(Picture {
                path: chosen.to_string(),
                name: picture_name(name),
                own: false,
            });
        }
        tiles.extend(pictures.into_iter().map(|p| {
            let thumbnail = self
                .thumbnails
                .iter()
                .find(|(path, _)| *path == p.path)
                .map(|(_, image)| image.clone());
            let tile = GalleryTile::new(p.name.clone(), thumbnail).description(p.path.clone());
            (tile, Choice::Picture(p.path))
        }));
        tiles
    }

    fn chosen_tile(&self, tiles: &[(GalleryTile, Choice)]) -> Option<usize> {
        let prefs = self.prefs.as_ref()?;
        let background = prefs.background();
        tiles
            .iter()
            .position(|(_, choice)| match (choice, background.picture) {
                (Choice::Picture(path), Some(chosen)) => path == chosen,
                (Choice::Gradient(id), None) => *id == background.gradient.id,
                _ => false,
            })
    }

    fn save(&mut self, change: impl FnOnce(&mut Prefs)) -> Vec<AppEffect> {
        let Some(prefs) = self.prefs.as_mut() else {
            return Vec::new();
        };
        let before = prefs.clone();
        change(prefs);
        let changes: Vec<Change> = Key::ALL
            .into_iter()
            .filter(|&key| {
                prefs.get(key) != before.get(key) || prefs.is_set(key) != before.is_set(key)
            })
            .map(|key| (key, prefs.is_set(key).then(|| prefs.get(key).to_string())))
            .collect();
        if changes.is_empty() {
            return Vec::new();
        }
        self.said = None;
        self.saving += 1;
        vec![AppEffect::Desktop(Effect::Save(changes))]
    }

    fn own_chosen(&self) -> Option<&Picture> {
        let chosen = self.picture()?;
        self.pictures.iter().find(|p| p.own && p.path == chosen)
    }

    fn add_error(&self) -> Option<&'static str> {
        let typed = self.adding.as_ref()?.text().trim();
        match typed.is_empty() || typed.starts_with('/') || typed.starts_with('~') {
            true => None,
            false => Some("Type the picture's whole path, starting with a slash."),
        }
    }

    /// Thumbnails for the pictures shown that have none asked for, and none
    /// kept for a picture no longer shown.
    fn thumbnail_what_is_new(&mut self) -> Vec<AppEffect> {
        let shown: Vec<String> = self
            .tiles()
            .into_iter()
            .filter_map(|(_, choice)| match choice {
                Choice::Picture(path) => Some(path),
                Choice::Gradient(_) => None,
            })
            .collect();
        self.thumbnails.retain(|(path, _)| shown.contains(path));
        self.asked.retain(|path| shown.contains(path));
        let new: Vec<String> = shown
            .into_iter()
            .filter(|path| !self.asked.contains(path))
            .collect();
        if new.is_empty() {
            return Vec::new();
        }
        self.asked.extend(new.iter().cloned());
        vec![AppEffect::Desktop(Effect::Thumbnails(new))]
    }

    pub fn update(&mut self, msg: Msg) -> Vec<AppEffect> {
        match msg {
            Msg::Prefs(_) if self.saving > 0 => {}
            Msg::Prefs(prefs) => {
                self.prefs = Some(prefs);
                return self.thumbnail_what_is_new();
            }
            Msg::Pictures(pictures) => {
                self.pictures = pictures;
                return self.thumbnail_what_is_new();
            }
            Msg::Thumbnail(path, image) => {
                self.thumbnails.retain(|(p, _)| *p != path);
                self.thumbnails.push((path, image));
            }
            Msg::Choose(at) => {
                let Some((_, choice)) = self.tiles().into_iter().nth(at) else {
                    return Vec::new();
                };
                return self.save(|prefs| match choice {
                    Choice::Gradient(id) => {
                        let _ = prefs.set(Key::BackgroundGradient, id);
                        prefs.reset(Key::BackgroundPicture);
                    }
                    Choice::Picture(path) => {
                        let _ = prefs.set(Key::BackgroundPicture, &path);
                    }
                });
            }
            Msg::Fit(row) => {
                let Some(fit) = Fit::ALL.get(row) else {
                    return Vec::new();
                };
                return self.save(|prefs| {
                    let _ = prefs.set(Key::BackgroundFit, fit.name());
                });
            }
            Msg::Clock => {
                return self.save(|prefs| {
                    let next = match prefs.twelve_hour_clock() {
                        true => "24h",
                        false => "12h",
                    };
                    let _ = prefs.set(Key::ClockFormat, next);
                });
            }
            Msg::Add => self.adding = Some(Field::default()),
            Msg::Path(input) => {
                let Some(field) = self.adding.as_mut() else {
                    return Vec::new();
                };
                if field.edit(input) == Left::Enter {
                    return self.update(Msg::Confirm);
                }
            }
            Msg::Cancel => {
                self.adding = None;
                self.removing = None;
            }
            Msg::Confirm => {
                let typed = self.adding.as_ref().map(|f| f.text().trim().to_string());
                let Some(path) = typed.filter(|t| !t.is_empty() && self.add_error().is_none())
                else {
                    return Vec::new();
                };
                self.adding = None;
                self.said = None;
                return vec![AppEffect::Desktop(Effect::Add(path))];
            }
            Msg::Added(Ok(path)) => {
                let mut effects = self.save(|prefs| {
                    let _ = prefs.set(Key::BackgroundPicture, &path);
                });
                effects.push(AppEffect::Desktop(Effect::Read));
                return effects;
            }
            Msg::Added(Err(why)) => self.said = Some((Tone::Danger, why)),
            Msg::Remove => self.removing = self.own_chosen().cloned(),
            Msg::ConfirmRemove => {
                let Some(picture) = self.removing.take() else {
                    return Vec::new();
                };
                let mut effects = self.save(|prefs| prefs.reset(Key::BackgroundPicture));
                effects.push(AppEffect::Desktop(Effect::Remove(picture.path)));
                effects.push(AppEffect::Desktop(Effect::Read));
                return effects;
            }
            Msg::Saved(saved) => {
                self.saving = self.saving.saturating_sub(1);
                match saved {
                    Ok(prefs) if self.saving == 0 => self.prefs = Some(prefs),
                    Ok(_) => {}
                    Err(why) => {
                        self.said = Some((Tone::Danger, why));
                        if self.saving == 0 {
                            return vec![AppEffect::Desktop(Effect::Read)];
                        }
                    }
                }
            }
        }
        Vec::new()
    }

    pub fn view(&self) -> Node<AppMsg> {
        let about =
            "Your background and your clock. Your desktop takes each change as you make it.";
        let Some(prefs) = &self.prefs else {
            let reading = Notice::new(Tone::Info, "Reading your preferences");
            return crate::page(Panel::Desktop, about, vec![reading.into()], None);
        };
        let tiles = self.tiles();
        let chosen = self.chosen_tile(&tiles);
        let mut buttons = vec![
            Button::new("Add picture…")
                .on_press(AppMsg::Desktop(Msg::Add))
                .into(),
        ];
        if self.own_chosen().is_some() {
            buttons.push(
                Button::new("Remove picture")
                    .on_press(AppMsg::Desktop(Msg::Remove))
                    .into(),
            );
        }
        let mut parts: Vec<Node<AppMsg>> = Vec::new();
        if let Some((tone, said)) = &self.said {
            parts.push(Notice::new(*tone, said.clone()).into());
        }
        parts.push(
            Section::new(
                "Background",
                column(
                    space::MD,
                    vec![
                        Gallery::new(tiles.into_iter().map(|(tile, _)| tile).collect())
                            .selected(chosen)
                            .on_select(|at| AppMsg::Desktop(Msg::Choose(at)))
                            .into(),
                        row(space::SM, buttons),
                    ],
                ),
            )
            .into(),
        );
        let background = prefs.background();
        if background.picture.is_some() {
            let rows = Fit::ALL.iter().map(|f| ListRow::new(f.label())).collect();
            parts.push(
                Section::new(
                    "How the picture fits",
                    List::new(rows)
                        .selected(Fit::ALL.iter().position(|&f| f == background.fit))
                        .on_select(|row| AppMsg::Desktop(Msg::Fit(row))),
                )
                .into(),
            );
        }
        parts.push(
            Section::new(
                "Clock",
                Node::Checkbox {
                    checked: !prefs.twelve_hour_clock(),
                    label: String::from("Use a 24-hour clock"),
                    on_toggle: Some(AppMsg::Desktop(Msg::Clock)),
                    enabled: true,
                },
            )
            .into(),
        );
        crate::page(Panel::Desktop, about, parts, None)
    }

    pub fn dialog(&self) -> Option<Node<AppMsg>> {
        if let Some(picture) = &self.removing {
            return Some(
                Dialog::new(format!("Remove {}?", picture.name))
                    .content(
                        Text::new(
                            "It's taken away from your pictures for good, and your background goes back to a gradient.",
                        )
                        .tone(Tone::Secondary)
                        .wrap(),
                    )
                    .footer(
                        ButtonRow::new()
                            .cancel(Button::new("Cancel").on_press(AppMsg::Desktop(Msg::Cancel)))
                            .destructive(
                                Button::new("Remove")
                                    .on_press(AppMsg::Desktop(Msg::ConfirmRemove)),
                            ),
                    )
                    .into(),
            );
        }
        let field = self.adding.as_ref()?;
        let typed = !field.text().trim().is_empty();
        let path = field.line("/home/user/Pictures/beach.png", |i| {
            AppMsg::Desktop(Msg::Path(i))
        });
        Some(
            Dialog::new("Add a picture")
                .content(column(
                    space::MD,
                    vec![
                        Text::new("Type where the PNG picture is. A copy of it is kept with your pictures.")
                            .tone(Tone::Secondary)
                            .wrap()
                            .into(),
                        with_error(path, self.add_error()),
                    ],
                ))
                .footer(
                    ButtonRow::new()
                        .cancel(Button::new("Cancel").on_press(AppMsg::Desktop(Msg::Cancel)))
                        .confirm(
                            Button::new("Add")
                                .on_press(AppMsg::Desktop(Msg::Confirm))
                                .enabled(typed && self.add_error().is_none()),
                        ),
                )
                .into(),
        )
    }
}

#[derive(Clone, Debug)]
enum Choice {
    Gradient(&'static str),
    Picture(String),
}

/// What a picture is called, from its file's name: `beach-at-dawn.png`
/// reads as `Beach at dawn`.
pub fn picture_name(file: &str) -> String {
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    let words = stem.replace(['-', '_'], " ");
    let mut chars = words.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::from("Picture"),
    }
}
