// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{command_line as cli, errors::Errors};
use fallible::copy_from_slice::copy_slice_to_vec;
use move_ir_types::location::*;
use petgraph::{algo::astar as petgraph_astar, graphmap::DiGraphMap};
use std::{
    convert::TryFrom,
    fmt,
    hash::Hash,
    sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
};
use structopt::*;

pub mod ast_debug;
pub mod remembering_unique_map;
pub mod unique_map;
pub mod unique_set;

//**************************************************************************************************
// Address
//**************************************************************************************************

pub const ADDRESS_LENGTH: usize = 16;

#[derive(Ord, PartialOrd, Eq, PartialEq, Hash, Default, Clone, Copy)]
pub struct Address([u8; ADDRESS_LENGTH]);

impl Address {
    pub const DIEM_CORE: Address = Address::new([
        0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 1u8,
    ]);

    pub const fn new(address: [u8; ADDRESS_LENGTH]) -> Self {
        Address(address)
    }

    pub fn to_u8(self) -> [u8; ADDRESS_LENGTH] {
        self.0
    }

    pub fn parse_str(s: &str) -> Result<Address, String> {
        let mut hex_string = String::from(&s[2..]);
        if hex_string.len() % 2 != 0 {
            hex_string.insert(0, '0');
        }

        let mut result = hex::decode(hex_string.as_str())
            .map_err(|e| format!("hex string {} fails to decode with Error {}", hex_string, e))?;
        let len = result.len();
        if len < ADDRESS_LENGTH {
            result.reverse();
            result.resize(ADDRESS_LENGTH, 0);
            result.reverse();
        }

        assert!(result.len() >= ADDRESS_LENGTH);
        Self::try_from(&result[..]).map_err(|_| {
            format!(
                "Address is {} bytes long. The maximum size is {} bytes",
                result.len(),
                ADDRESS_LENGTH
            )
        })
    }
}

impl AsRef<[u8]> for Address {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter) -> std::fmt::Result {
        write!(f, "0x{:#X}", self)
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:#X}", self)
    }
}

impl fmt::LowerHex for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let encoded = hex::encode(&self.0);
        let dropped = encoded
            .chars()
            .skip_while(|c| c == &'0')
            .collect::<String>();
        if dropped.is_empty() {
            write!(f, "0")
        } else {
            write!(f, "{}", dropped)
        }
    }
}

impl fmt::UpperHex for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let encoded = hex::encode_upper(&self.0);
        let dropped = encoded
            .chars()
            .skip_while(|c| c == &'0')
            .collect::<String>();
        if dropped.is_empty() {
            write!(f, "0")
        } else {
            write!(f, "{}", dropped)
        }
    }
}

impl TryFrom<&[u8]> for Address {
    type Error = String;

    fn try_from(bytes: &[u8]) -> Result<Address, String> {
        if bytes.len() != ADDRESS_LENGTH {
            Err(format!("The Address {:?} is of invalid length", bytes))
        } else {
            let mut addr = [0u8; ADDRESS_LENGTH];
            copy_slice_to_vec(bytes, &mut addr).map_err(|e| format!("{}", e))?;
            Ok(Address(addr))
        }
    }
}

//**************************************************************************************************
// Name
//**************************************************************************************************

pub trait TName: Eq + Ord + Clone {
    type Key: Ord + Clone;
    type Loc: Copy;
    fn drop_loc(self) -> (Self::Loc, Self::Key);
    fn add_loc(loc: Self::Loc, key: Self::Key) -> Self;
    fn borrow(&self) -> (&Self::Loc, &Self::Key);
}

pub trait Identifier {
    fn value(&self) -> &str;
    fn loc(&self) -> Loc;
}

// TODO maybe we should intern these strings somehow
pub type Name = Spanned<String>;

impl TName for Name {
    type Key = String;
    type Loc = Loc;

    fn drop_loc(self) -> (Loc, String) {
        (self.loc, self.value)
    }

    fn add_loc(loc: Loc, key: String) -> Self {
        sp(loc, key)
    }

    fn borrow(&self) -> (&Loc, &String) {
        (&self.loc, &self.value)
    }
}

//**************************************************************************************************
// Graphs
//**************************************************************************************************

pub fn shortest_cycle<'a, T: Ord + Hash>(
    dependency_graph: &DiGraphMap<&'a T, ()>,
    start: &'a T,
) -> Vec<&'a T> {
    let shortest_path = dependency_graph
        .neighbors(start)
        .fold(None, |shortest_path, neighbor| {
            let path_opt = petgraph_astar(
                dependency_graph,
                neighbor,
                |finish| finish == start,
                |_e| 1,
                |_| 0,
            );
            match (shortest_path, path_opt) {
                (p, None) | (None, p) => p,
                (Some((acc_len, acc_path)), Some((cur_len, cur_path))) => {
                    Some(if cur_len < acc_len {
                        (cur_len, cur_path)
                    } else {
                        (acc_len, acc_path)
                    })
                }
            }
        });
    let (_, mut path) = shortest_path.unwrap();
    path.insert(0, start);
    path
}

//**************************************************************************************************
// Compilation Env
//**************************************************************************************************

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompilationEnv {
    flags: Flags,
    errors: Errors,
    // TODO(tzakian): Remove the global counter and use this counter instead
    // pub counter: u64,
}

impl CompilationEnv {
    pub fn new(flags: Flags) -> Self {
        Self {
            flags,
            errors: Vec::new(),
        }
    }

    pub fn add_error(&mut self, e: Vec<(Loc, impl Into<String>)>) {
        self.errors
            .push(e.into_iter().map(|(loc, msg)| (loc, msg.into())).collect())
    }

    pub fn add_errors(&mut self, es: Errors) {
        self.errors.extend(es)
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    pub fn count_errors(&self) -> usize {
        self.errors.len()
    }

    pub fn check_errors(&mut self) -> Result<(), Errors> {
        if self.has_errors() {
            Err(std::mem::take(&mut self.errors))
        } else {
            Ok(())
        }
    }

    pub fn flags(&self) -> &Flags {
        &self.flags
    }
}

//**************************************************************************************************
// Counter
//**************************************************************************************************

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Counter(usize);

impl Counter {
    pub fn next() -> u64 {
        static COUNTER_NEXT: AtomicUsize = AtomicUsize::new(0);

        COUNTER_NEXT.fetch_add(1, AtomicOrdering::AcqRel) as u64
    }
}

//**************************************************************************************************
// Display
//**************************************************************************************************

pub fn format_delim<T: fmt::Display, I: IntoIterator<Item = T>>(items: I, delim: &str) -> String {
    items
        .into_iter()
        .map(|item| format!("{}", item))
        .collect::<Vec<_>>()
        .join(delim)
}

pub fn format_comma<T: fmt::Display, I: IntoIterator<Item = T>>(items: I) -> String {
    format_delim(items, ", ")
}

//**************************************************************************************************
// Flags
//**************************************************************************************************

#[derive(Clone, Debug, Eq, PartialEq, StructOpt)]
pub struct Flags {
    /// Compile in test mode
    #[structopt(
        short = cli::TEST_SHORT,
        long = cli::TEST,
    )]
    test: bool,
}

impl Flags {
    pub fn empty() -> Self {
        Self { test: false }
    }

    pub fn testing() -> Self {
        Self { test: true }
    }

    pub fn is_testing(&self) -> bool {
        self.test
    }
}

//**************************************************************************************************
// Attributes
//**************************************************************************************************

pub mod known_attributes {
    #[derive(Debug, PartialEq, Clone, PartialOrd, Eq, Ord)]
    pub enum TestingAttributes {
        // Can be called by other testing code, and included in compilation in test mode
        TestOnly,
        // Is a test that will be run
        Test,
        // This test is expected to fail
        ExpectedFailure,
    }

    impl TestingAttributes {
        pub const TEST: &'static str = "test";
        pub const EXPECTED_FAILURE: &'static str = "expected_failure";
        pub const TEST_ONLY: &'static str = "test_only";
        pub const CODE_ASSIGNMENT_NAME: &'static str = "abort_code";

        pub fn resolve(attribute_str: &str) -> Option<Self> {
            Some(match attribute_str {
                Self::TEST => Self::Test,
                Self::TEST_ONLY => Self::TestOnly,
                Self::EXPECTED_FAILURE => Self::ExpectedFailure,
                _ => return None,
            })
        }

        pub fn name(&self) -> &str {
            match self {
                Self::Test => Self::TEST,
                Self::TestOnly => Self::TEST_ONLY,
                Self::ExpectedFailure => Self::EXPECTED_FAILURE,
            }
        }
    }
}
