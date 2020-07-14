// Copyright 2018 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::location::{DstLocation, SrcLocation};

use hex_fmt::HexFmt;
use std::{
    collections::BTreeSet,
    fmt::{self, Debug, Formatter},
};
use xor_name::{Prefix, XorName};

/// An Event raised as node complete joining
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Connected {
    /// Node first joining the network
    First,
    /// Node relocating from one section to another
    Relocate,
}

/// An Event raised by a `Node` or `Client` via its event sender.
///
/// These are sent by routing to the library's user. It allows the user to handle requests and
/// responses, and to react to changes in the network.
///
/// `Request` and `Response` events from section locations are only raised once the quorum has
/// been reached, i.e. enough members of the section have sent the same message.
#[derive(Clone, Eq, PartialEq)]
// FIXME - See https://maidsafe.atlassian.net/browse/MAID-2026 for info on removing this exclusion.
#[allow(clippy::large_enum_variant)]
pub enum Event {
    /// The node has successfully connected to the network.
    Connected(Connected),
    /// Received a message.
    MessageReceived {
        /// The content of the message.
        content: Vec<u8>,
        /// The source location that sent the message.
        src: SrcLocation,
        /// The destination location that receives the message.
        dst: DstLocation,
    },
    /// Consensus on a custom event.
    Consensus(Vec<u8>),
    /// The node has been promoted to elder
    Promoted,
    /// The node has been demoted from elder
    Demoted,
    /// A node joined our section.
    MemberJoined {
        /// Name of the node
        name: XorName,
        /// Age of the node
        age: u8,
    },
    /// A node left our section.
    MemberLeft {
        /// Name of the node
        name: XorName,
        /// Age of the node
        age: u8,
    },
    /// The set of elders in our section has changed.
    EldersChanged {
        /// The prefix of our section.
        prefix: Prefix,
        /// The BLS public key of our section.
        key: bls::PublicKey,
        /// The set of elders of our section.
        elders: BTreeSet<XorName>,
    },
    /// Disconnected or failed to connect - restart required.
    RestartRequired,
    /// Startup failed - terminate.
    Terminated,
}

impl Debug for Event {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        match self {
            Self::Connected(connect_type) => write!(formatter, "Connected({:?})", connect_type),
            Self::MessageReceived { content, src, dst } => write!(
                formatter,
                "MessageReceived {{ content: \"{:<8}\", src: {:?}, dst: {:?} }}",
                HexFmt(content),
                src,
                dst
            ),
            Self::Consensus(payload) => write!(formatter, "Consensus({:<8})", HexFmt(payload)),
            Self::Promoted => write!(formatter, "Promoted"),
            Self::Demoted => write!(formatter, "Demoted"),
            Self::MemberJoined { name, age } => formatter
                .debug_struct("MemberJoined")
                .field("name", name)
                .field("age", age)
                .finish(),
            Self::MemberLeft { name, age } => formatter
                .debug_struct("MemberLeft")
                .field("name", name)
                .field("age", age)
                .finish(),
            Self::EldersChanged {
                prefix,
                key,
                elders,
            } => formatter
                .debug_struct("EldersChanged")
                .field("prefix", prefix)
                .field("key", key)
                .field("elders", elders)
                .finish(),
            Self::RestartRequired => write!(formatter, "RestartRequired"),
            Self::Terminated => write!(formatter, "Terminated"),
        }
    }
}
