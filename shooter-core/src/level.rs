//! Levels as text: one character per cell, rows of equal length, closed on
//! every edge.
//!
//! | Char | Cell |
//! |---|---|
//! | `#` `%` `=` `+` | walls: plating, block, pipes, grate |
//! | `.` | floor |
//! | `\|` | a door anyone opens |
//! | `B` `A` | a door the blue or the amber keycard opens |
//! | `E` | the exit panel, a wall the player uses to leave |
//! | `>` `<` `^` `v` | the player's start, facing east, west, north, south |
//! | `c` `s` `W` | a crawler, a sentry, a warden |
//! | `h` `r` `b` `a` | a repair kit, a box of rivets, the blue card, the amber card |

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WallKind {
    Plating,
    Block,
    Pipes,
    Grate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lock {
    Open,
    Blue,
    Amber,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cell {
    Floor,
    Wall(WallKind),
    Door(Lock),
    Exit,
}

impl Cell {
    pub fn is_floor(self) -> bool {
        self == Cell::Floor
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnemyKind {
    Crawler,
    Sentry,
    Warden,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    RepairKit,
    Rivets,
    BlueCard,
    AmberCard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Thing {
    Enemy(EnemyKind),
    Item(ItemKind),
}

#[derive(Clone, Debug)]
pub struct Level {
    pub name: &'static str,
    pub width: usize,
    pub height: usize,
    pub cells: Vec<Cell>,
    /// Cell centre and facing, in radians; y grows southward.
    pub start: (f32, f32, f32),
    /// Each at its cell's centre.
    pub things: Vec<(Thing, f32, f32)>,
}

impl Level {
    pub fn cell(&self, x: i32, y: i32) -> Cell {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            return Cell::Wall(WallKind::Plating);
        }
        self.cells[y as usize * self.width + x as usize]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    Ragged { row: usize },
    Unknown { row: usize, col: usize, ch: char },
    OpenEdge { row: usize, col: usize },
    Starts(usize),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Empty => write!(f, "the map has no rows"),
            ParseError::Ragged { row } => write!(f, "row {row} is not as long as the first"),
            ParseError::Unknown { row, col, ch } => {
                write!(f, "row {row}, column {col}: {ch:?} is no cell")
            }
            ParseError::OpenEdge { row, col } => {
                write!(f, "row {row}, column {col}: the edge must be a wall")
            }
            ParseError::Starts(n) => write!(f, "the map has {n} starts, not one"),
        }
    }
}

pub fn parse(name: &'static str, rows: &[&str]) -> Result<Level, ParseError> {
    let height = rows.len();
    let width = rows.first().ok_or(ParseError::Empty)?.chars().count();
    let mut cells = Vec::with_capacity(width * height);
    let mut starts = Vec::new();
    let mut things = Vec::new();
    for (row, line) in rows.iter().enumerate() {
        if line.chars().count() != width {
            return Err(ParseError::Ragged { row });
        }
        for (col, ch) in line.chars().enumerate() {
            let centre = (col as f32 + 0.5, row as f32 + 0.5);
            let facing = |angle: f32| (centre.0, centre.1, angle);
            let thing = |t: Thing| Some((t, centre.0, centre.1));
            let (cell, start, placed) = match ch {
                '#' => (Cell::Wall(WallKind::Plating), None, None),
                '%' => (Cell::Wall(WallKind::Block), None, None),
                '=' => (Cell::Wall(WallKind::Pipes), None, None),
                '+' => (Cell::Wall(WallKind::Grate), None, None),
                '.' => (Cell::Floor, None, None),
                '|' => (Cell::Door(Lock::Open), None, None),
                'B' => (Cell::Door(Lock::Blue), None, None),
                'A' => (Cell::Door(Lock::Amber), None, None),
                'E' => (Cell::Exit, None, None),
                '>' => (Cell::Floor, Some(facing(0.0)), None),
                'v' => (Cell::Floor, Some(facing(std::f32::consts::FRAC_PI_2)), None),
                '<' => (Cell::Floor, Some(facing(std::f32::consts::PI)), None),
                '^' => (
                    Cell::Floor,
                    Some(facing(-std::f32::consts::FRAC_PI_2)),
                    None,
                ),
                'c' => (Cell::Floor, None, thing(Thing::Enemy(EnemyKind::Crawler))),
                's' => (Cell::Floor, None, thing(Thing::Enemy(EnemyKind::Sentry))),
                'W' => (Cell::Floor, None, thing(Thing::Enemy(EnemyKind::Warden))),
                'h' => (Cell::Floor, None, thing(Thing::Item(ItemKind::RepairKit))),
                'r' => (Cell::Floor, None, thing(Thing::Item(ItemKind::Rivets))),
                'b' => (Cell::Floor, None, thing(Thing::Item(ItemKind::BlueCard))),
                'a' => (Cell::Floor, None, thing(Thing::Item(ItemKind::AmberCard))),
                ch => return Err(ParseError::Unknown { row, col, ch }),
            };
            let edge = row == 0 || col == 0 || row == height - 1 || col == width - 1;
            if edge && matches!(cell, Cell::Floor | Cell::Door(_)) {
                return Err(ParseError::OpenEdge { row, col });
            }
            cells.push(cell);
            starts.extend(start);
            things.extend(placed);
        }
    }
    if starts.len() != 1 {
        return Err(ParseError::Starts(starts.len()));
    }
    Ok(Level {
        name,
        width,
        height,
        cells,
        start: starts[0],
        things,
    })
}

pub struct Source {
    pub name: &'static str,
    pub rows: &'static [&'static str],
}

pub const LEVELS: &[Source] = &[
    Source {
        name: "Intake",
        rows: &[
            "########################",
            "#>.....#........#......#",
            "#......|...c....|...r..#",
            "#......#........#......#",
            "#..r...#####|####......#",
            "#......#........#..c...#",
            "####|###...h....#......#",
            "#......#........####|###",
            "#..c...#...c....#......#",
            "#......####|#####......#",
            "#......+..........c....#",
            "#..h...+...............#",
            "#......%%%%%%|%%%%%%%%%#",
            "#......%...........r...#",
            "#..c...|....c......c...E",
            "#......%...............#",
            "########################",
        ],
    },
    Source {
        name: "Pump hall",
        rows: &[
            "==========================",
            "=v.....=......s.....=....=",
            "=......=............=.r..=",
            "=..r...|....====....|....=",
            "=......=....=..=....=..c.=",
            "===|====....=b.=....======",
            "=......=....=..=....=....=",
            "=..c...=....==|=....B....E",
            "=......=............=..s.=",
            "=.h....=..c.....c...=....=",
            "=......====|=========....=",
            "=..........s........=.h..=",
            "=..r......c.........=....=",
            "==========================",
        ],
    },
    Source {
        name: "Reactor core",
        rows: &[
            "############################",
            "#^.....%.......s......%....#",
            "#......%..............%..r.#",
            "#..r...|.....++++.....|....#",
            "#......%.....+..+.....%..c.#",
            "%%%|%%%%.....+b.+.....%%%|%#",
            "#......%.....++|+.....%....#",
            "#..c...%..............%..h.#",
            "#......%..c........c..%....#",
            "#..h...%%%%%%%B%%%%%%%%....#",
            "#......%.......a......%....#",
            "#......|..s........s..|..c.#",
            "#......%..............%....#",
            "%%%%A%%%%%%%%%%%%%%%%%%%%%%#",
            "#.......c.......c..........#",
            "#..r.......W.........h..r..#",
            "#.......c..........c.......E",
            "############################",
        ],
    },
];

pub fn load(index: usize) -> Level {
    let source = &LEVELS[index];
    parse(source.name, source.rows).expect("every shipped level parses")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Every cell the player reaches from the start, holding the keys given.
    fn reachable(level: &Level, blue: bool, amber: bool) -> Vec<bool> {
        let mut seen = vec![false; level.cells.len()];
        let start = (level.start.0 as usize, level.start.1 as usize);
        let mut queue = VecDeque::from([start]);
        seen[start.1 * level.width + start.0] = true;
        while let Some((x, y)) = queue.pop_front() {
            for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                let passable = match level.cell(nx, ny) {
                    Cell::Floor | Cell::Door(Lock::Open) => true,
                    Cell::Door(Lock::Blue) => blue,
                    Cell::Door(Lock::Amber) => amber,
                    Cell::Wall(_) | Cell::Exit => false,
                };
                let i = ny as usize * level.width + nx as usize;
                if passable && !seen[i] {
                    seen[i] = true;
                    queue.push_back((nx as usize, ny as usize));
                }
            }
        }
        seen
    }

    fn holds(level: &Level, seen: &[bool], item: ItemKind) -> bool {
        level
            .things
            .iter()
            .any(|&(t, x, y)| t == Thing::Item(item) && seen[y as usize * level.width + x as usize])
    }

    #[test]
    fn every_level_parses_and_its_exit_is_reachable_with_the_keys_it_holds() {
        for (index, source) in LEVELS.iter().enumerate() {
            let level = parse(source.name, source.rows).unwrap();
            let (mut blue, mut amber) = (false, false);
            let seen = loop {
                let seen = reachable(&level, blue, amber);
                let more = (
                    blue || holds(&level, &seen, ItemKind::BlueCard),
                    amber || holds(&level, &seen, ItemKind::AmberCard),
                );
                if more == (blue, amber) {
                    break seen;
                }
                (blue, amber) = more;
            };
            let exit_touched = (0..level.height as i32).any(|y| {
                (0..level.width as i32).any(|x| {
                    level.cell(x, y) == Cell::Exit
                        && [(1, 0), (-1, 0), (0, 1), (0, -1)].iter().any(|(dx, dy)| {
                            let (nx, ny) = (x + dx, y + dy);
                            level.cell(nx, ny) != Cell::Exit
                                && nx >= 0
                                && ny >= 0
                                && (nx as usize) < level.width
                                && (ny as usize) < level.height
                                && seen[ny as usize * level.width + nx as usize]
                        })
                })
            });
            assert!(
                exit_touched,
                "level {index} ({}): no way to the exit",
                source.name
            );
        }
    }

    #[test]
    fn a_ragged_or_open_map_is_refused() {
        assert_eq!(
            parse("t", &["###", "#>", "###"]).unwrap_err(),
            ParseError::Ragged { row: 1 }
        );
        assert_eq!(
            parse("t", &["###", ">.#", "###"]).unwrap_err(),
            ParseError::OpenEdge { row: 1, col: 0 }
        );
        assert_eq!(
            parse("t", &["###", "#.#", "###"]).unwrap_err(),
            ParseError::Starts(0)
        );
        assert_eq!(
            parse("t", &["###", "#?#", "###"]).unwrap_err(),
            ParseError::Unknown {
                row: 1,
                col: 1,
                ch: '?'
            }
        );
    }

    #[test]
    fn a_start_faces_where_its_arrow_points() {
        let level = parse("t", &["###", "#v#", "###"]).unwrap();
        assert_eq!(level.start, (1.5, 1.5, std::f32::consts::FRAC_PI_2));
    }
}
