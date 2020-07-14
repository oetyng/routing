// Copyright 2020 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::utils::{self as test_utils, MockTransport};
use crate::{
    consensus::{self, AccumulatingEvent, ParsecRequest},
    error::Result,
    id::{FullId, P2pNode, PublicId},
    location::DstLocation,
    messages::{
        AccumulatingMessage, BootstrapResponse, Message, PlainMessage, SrcAuthority, Variant,
    },
    node::{Node, NodeConfig},
    rng::{self, MainRng},
    section::{
        member_info, EldersInfo, MemberState, SectionKeyShare, SectionProofChain, SharedState,
        MIN_AGE,
    },
    utils, ELDER_SIZE,
};
use itertools::Itertools;
use mock_quic_p2p::Network;
use rand::Rng;
use std::{collections::BTreeSet, iter, net::SocketAddr};
use xor_name::{Prefix, XorName};

// Minimal number of votes to reach accumulation.
const ACCUMULATE_VOTE_COUNT: usize = 5;
// Only one vote missing to reach accumulation.
const NOT_ACCUMULATE_ALONE_VOTE_COUNT: usize = 4;

struct DkgToSectionInfo {
    new_pk_set: bls::PublicKeySet,
    new_other_ids: Vec<(FullId, bls::SecretKeyShare)>,
    new_elders_info: EldersInfo,
}

struct Env {
    pub rng: MainRng,
    pub network: Network,
    pub subject: Node,
    pub other_ids: Vec<(FullId, bls::SecretKeyShare)>,
    pub elders_info: EldersInfo,
    pub public_key_set: bls::PublicKeySet,
    pub candidate: P2pNode,
}

impl Env {
    fn new(sec_size: usize) -> Self {
        let mut rng = rng::new();
        let network = Network::new();

        let (proven_elders_info, full_ids) =
            test_utils::create_elders_info(&mut rng, &network, sec_size);
        let elders_info = proven_elders_info.value.clone();

        let secret_key_set = consensus::generate_secret_key_set(&mut rng, full_ids.len());
        let public_key_set = secret_key_set.public_keys();
        let public_key = public_key_set.public_key();

        let mut full_and_bls_ids = full_ids
            .into_iter()
            .enumerate()
            .map(|(idx, (_, full_id))| (full_id, secret_key_set.secret_key_share(idx)))
            .collect_vec();

        let (full_id, secret_key_share) = full_and_bls_ids.remove(0);
        let other_ids = full_and_bls_ids;

        let mut shared_state = SharedState::new(proven_elders_info, public_key);
        for p2p_node in elders_info.elders.values() {
            let proof = test_utils::create_proof(
                &secret_key_set,
                &member_info::to_sign(p2p_node.name(), MemberState::Joined),
            );
            shared_state
                .our_members
                .add(p2p_node.clone(), MIN_AGE, proof);
        }

        let section_key_share = SectionKeyShare {
            public_key_set,
            index: elders_info.position(full_id.public_id().name()).unwrap(),
            secret_key_share,
        };

        let (subject, ..) = Node::approved(
            NodeConfig {
                full_id: Some(full_id),
                ..Default::default()
            },
            shared_state,
            0,
            Some(section_key_share),
        );

        let candidate = Peer::gen(&mut rng, &network).to_p2p_node();

        let mut env = Self {
            rng,
            network,
            subject,
            other_ids,
            elders_info,
            public_key_set: secret_key_set.public_keys(),
            candidate,
        };

        // Process initial unpolled event including genesis
        env.n_vote_for_unconsensused_events(env.other_ids.len());
        env.create_gossip().unwrap();
        env
    }

    fn poll(&mut self) {
        self.network.poll(&mut self.rng)
    }

    fn n_vote_for(&mut self, count: usize, events: impl IntoIterator<Item = AccumulatingEvent>) {
        assert!(count <= self.other_ids.len());

        let parsec = self
            .subject
            .consensus_engine_mut()
            .unwrap()
            .parsec_map_mut();

        for event in events {
            for (full_id, secret_key_share) in self.other_ids.iter().take(count) {
                let event = if event.needs_signature() {
                    let index = self
                        .elders_info
                        .position(full_id.public_id().name())
                        .unwrap();
                    event
                        .clone()
                        .into_signed_network_event(&SectionKeyShare {
                            public_key_set: self.public_key_set.clone(),
                            index,
                            secret_key_share: secret_key_share.clone(),
                        })
                        .unwrap()
                } else {
                    event.clone().into_unsigned_network_event()
                };

                info!("Vote as {:?} for event {:?}", full_id.public_id(), event);
                parsec.vote_for_as(event.into_obs(), full_id);
            }
        }
    }

    fn n_vote_for_unconsensused_events(&mut self, count: usize) {
        let parsec = self
            .subject
            .consensus_engine_mut()
            .unwrap()
            .parsec_map_mut();
        let events = parsec.our_unpolled_observations().cloned().collect_vec();
        for event in events {
            self.other_ids.iter().take(count).for_each(|(full_id, _)| {
                info!(
                    "Vote as {:?} for unconsensused event {:?}",
                    full_id.public_id(),
                    event
                );
                parsec.vote_for_as(event.clone(), full_id);
            });
        }
    }

    fn create_gossip(&mut self) -> Result<()> {
        let other_full_id = &self.other_ids[0].0;
        let addr: SocketAddr = "127.0.0.3:9999".parse().unwrap();
        let parsec_version = self.subject.consensus_engine()?.parsec_version();
        let request = ParsecRequest::new();
        let message = Message::single_src(
            other_full_id,
            DstLocation::Direct,
            None,
            Variant::ParsecRequest(parsec_version, request),
        )
        .unwrap();

        self.subject.dispatch_message(Some(addr), message)
    }

    fn n_vote_for_gossipped(
        &mut self,
        count: usize,
        events: impl IntoIterator<Item = AccumulatingEvent>,
    ) -> Result<()> {
        self.n_vote_for(count, events);
        self.create_gossip()
    }

    fn accumulate_online(&mut self, p2p_node: P2pNode) {
        let _ = self.n_vote_for_gossipped(
            ACCUMULATE_VOTE_COUNT,
            iter::once(AccumulatingEvent::Online {
                p2p_node,
                previous_name: None,
                age: MIN_AGE,
                their_knowledge: None,
            }),
        );
    }

    fn updated_other_ids(&mut self, new_elders_info: EldersInfo) -> DkgToSectionInfo {
        let participants: BTreeSet<_> = new_elders_info.elder_ids().copied().collect();
        let parsec = self
            .subject
            .consensus_engine_mut()
            .unwrap()
            .parsec_map_mut();

        let dkg_results = self
            .other_ids
            .iter()
            .map(|(full_id, _)| {
                (
                    full_id.clone(),
                    parsec
                        .get_dkg_result_as(participants.clone(), full_id)
                        .expect("failed to get DKG result"),
                )
            })
            .collect_vec();

        let new_pk_set = dkg_results
            .first()
            .expect("no DKG results")
            .1
            .public_key_set
            .clone();
        let new_other_ids = dkg_results
            .into_iter()
            .filter_map(|(full_id, result)| (result.secret_key_share.map(|share| (full_id, share))))
            .collect_vec();
        DkgToSectionInfo {
            new_pk_set,
            new_other_ids,
            new_elders_info,
        }
    }

    fn accumulate_our_key_and_section_info_if_vote(&mut self, new_info: &DkgToSectionInfo) {
        let _ = self.n_vote_for_gossipped(
            NOT_ACCUMULATE_ALONE_VOTE_COUNT,
            vec![
                AccumulatingEvent::OurKey {
                    prefix: new_info.new_elders_info.prefix,
                    key: new_info.new_pk_set.public_key(),
                },
                AccumulatingEvent::SectionInfo(new_info.new_elders_info.clone()),
            ],
        );
    }

    fn accumulate_voted_unconsensused_events(&mut self) {
        self.n_vote_for_unconsensused_events(ACCUMULATE_VOTE_COUNT);
        let _ = self.create_gossip();
    }

    fn accumulate_offline(&mut self, offline_payload: XorName) {
        let _ = self.n_vote_for_gossipped(
            ACCUMULATE_VOTE_COUNT,
            iter::once(AccumulatingEvent::Offline(offline_payload)),
        );
    }

    // Accumulate `TheirKnowledge` for the latest version of our own section.
    fn accumulate_self_their_knowledge(&mut self) {
        let event = AccumulatingEvent::TheirKnowledge {
            prefix: self.elders_info.prefix,
            knowledge: self
                .subject
                .our_history()
                .expect("subject is not approved")
                .last_key_index(),
        };

        let _ = self.n_vote_for_gossipped(ACCUMULATE_VOTE_COUNT, iter::once(event));
    }

    // Accumulate `ParsecPrune` which should result in the parsec version increase.
    fn accumulate_parsec_prune(&mut self) {
        let _ = self.n_vote_for_gossipped(
            ACCUMULATE_VOTE_COUNT,
            iter::once(AccumulatingEvent::ParsecPrune),
        );
    }

    fn new_elders_info_with_candidate(&mut self) -> DkgToSectionInfo {
        assert!(
            self.elders_info.elders.len() < ELDER_SIZE,
            "There is already ELDER_SIZE elders - the candidate won't be promoted"
        );

        let new_elder_info = EldersInfo::new(
            self.elders_info
                .elders
                .values()
                .chain(iter::once(&self.candidate))
                .map(|node| (*node.public_id().name(), node.clone()))
                .collect(),
            self.elders_info.prefix,
        );

        self.updated_other_ids(new_elder_info)
    }

    fn new_elders_info_without_candidate(&mut self) -> DkgToSectionInfo {
        let new_elder_info = self.elders_info.clone();
        self.updated_other_ids(new_elder_info)
    }

    // Returns the EldersInfo after an elder is dropped and an adult is promoted to take
    // its place.
    fn new_elders_info_after_offline_and_promote(
        &mut self,
        dropped_elder_name: &XorName,
        promoted_adult_node: P2pNode,
    ) -> DkgToSectionInfo {
        assert!(
            self.other_ids
                .iter()
                .any(|(full_id, _)| { full_id.public_id().name() == dropped_elder_name }),
            "dropped node must be one of the other elders"
        );

        let new_member_map = self
            .elders_info
            .elders
            .iter()
            .filter(|(_, node)| node.name() != dropped_elder_name)
            .map(|(name, node)| (*name, node.clone()))
            .chain(iter::once((
                *promoted_adult_node.name(),
                promoted_adult_node,
            )))
            .collect();

        let new_elders_info = EldersInfo::new(new_member_map, self.elders_info.prefix);
        self.updated_other_ids(new_elders_info)
    }

    fn has_unpolled_observations(&self) -> bool {
        self.subject.has_unpolled_observations()
    }

    fn is_candidate_member(&self) -> bool {
        self.subject.is_peer_our_member(self.candidate.name())
    }

    fn is_candidate_elder(&self) -> bool {
        self.subject.is_peer_our_elder(self.candidate.name())
    }

    fn gen_peer(&mut self) -> Peer {
        Peer::gen(&mut self.rng, &self.network)
    }

    // Drop an existing elder and promote an adult to take its place. Drive the whole process to
    // completion by casting all necessary votes and letting them accumulate.
    fn perform_offline_and_promote(
        &mut self,
        dropped_elder_name: XorName,
        promoted_adult_node: P2pNode,
    ) {
        let new_info = self
            .new_elders_info_after_offline_and_promote(&dropped_elder_name, promoted_adult_node);

        self.accumulate_offline(dropped_elder_name);
        self.accumulate_our_key_and_section_info_if_vote(&new_info);
        self.accumulate_voted_unconsensused_events();

        self.elders_info = new_info.new_elders_info;
        self.other_ids = new_info.new_other_ids;
        self.public_key_set = new_info.new_pk_set;

        self.accumulate_self_their_knowledge();
    }

    fn accumulate_message(&self, dst: DstLocation, variant: Variant) -> Message {
        let state = self
            .subject
            .shared_state()
            .expect("subject is not approved");
        let proof_chain = state.prove(&dst, None);
        let dst_key = *state.section_key_by_location(&dst);

        let content = PlainMessage {
            src: *state.our_prefix(),
            dst,
            dst_key,
            variant,
        };

        let public_key_set = self.subject.public_key_set().unwrap();
        let accumulating_msgs = self.secret_key_shares().map(|(index, secret_key_share)| {
            let proof_share = content
                .prove(public_key_set.clone(), index, secret_key_share)
                .unwrap();
            AccumulatingMessage::new(content.clone(), proof_chain.clone(), proof_share)
        });
        test_utils::accumulate_messages(accumulating_msgs)
    }

    // Create `MockTransport` for the `index`-th other elder.
    fn create_transport_for_other_elder(&self, index: usize) -> MockTransport {
        let name = self.other_ids[index].0.public_id().name();
        let addr = self
            .elders_info
            .elders
            .get(name)
            .map(|p2p_node| p2p_node.peer_addr());

        MockTransport::new(addr)
    }

    fn sign_by_section(&self, payload: &[u8]) -> (bls::PublicKey, bls::Signature) {
        let public_key_set = self.subject.public_key_set().unwrap();
        let signature_shares: Vec<_> = self
            .secret_key_shares()
            .map(|(_, sk_share)| sk_share.sign(payload))
            .collect();
        let signature = public_key_set
            .combine_signatures(signature_shares.iter().enumerate())
            .unwrap();

        (public_key_set.public_key(), signature)
    }

    // Returns the secret key shares of all the nodes together with the node indices.
    fn secret_key_shares(&self) -> impl Iterator<Item = (usize, &bls::SecretKeyShare)> {
        iter::once((
            self.subject.name(),
            self.subject.secret_key_share().unwrap(),
        ))
        .chain(
            self.other_ids
                .iter()
                .map(|(full_id, sk_share)| (full_id.public_id().name(), sk_share)),
        )
        .map(move |(name, sk_share)| {
            let index = self.elders_info.position(name).unwrap();
            (index, sk_share)
        })
    }
}

struct Peer {
    full_id: FullId,
    addr: SocketAddr,
}

impl Peer {
    fn gen(rng: &mut MainRng, network: &Network) -> Self {
        Self {
            full_id: FullId::gen(rng),
            addr: network.gen_addr(),
        }
    }

    fn to_p2p_node(&self) -> P2pNode {
        P2pNode::new(*self.full_id.public_id(), self.addr)
    }
}

#[test]
fn construct() {
    let env = Env::new(ELDER_SIZE - 1);

    assert!(!env.has_unpolled_observations());
    assert!(!env.is_candidate_elder());
}

#[test]
fn when_accumulate_online_then_node_is_added_to_our_members() {
    let mut env = Env::new(ELDER_SIZE - 1);
    env.accumulate_online(env.candidate.clone());

    assert!(!env.has_unpolled_observations());
    assert!(env.is_candidate_member());
    assert!(!env.is_candidate_elder());
}

#[test]
#[ignore] //FIXME DKG is no longer carried out by parsec
fn when_accumulate_online_and_start_dkg_and_section_info_then_node_is_added_to_our_elders() {
    let mut env = Env::new(ELDER_SIZE - 1);
    let new_info = env.new_elders_info_with_candidate();
    env.accumulate_online(env.candidate.clone());

    env.accumulate_our_key_and_section_info_if_vote(&new_info);
    env.accumulate_voted_unconsensused_events();

    assert!(!env.has_unpolled_observations());
    assert!(env.is_candidate_member());
    assert!(env.is_candidate_elder());
}

#[test]
#[ignore] //FIXME DKG is no longer carried out by parsec
fn when_accumulate_offline_then_node_is_removed_from_our_members() {
    let mut env = Env::new(ELDER_SIZE - 1);
    let info1 = env.new_elders_info_with_candidate();
    env.accumulate_online(env.candidate.clone());
    env.accumulate_our_key_and_section_info_if_vote(&info1);
    env.accumulate_voted_unconsensused_events();

    env.other_ids = info1.new_other_ids;
    env.elders_info = info1.new_elders_info;
    env.public_key_set = info1.new_pk_set;

    env.accumulate_offline(*env.candidate.name());

    assert!(!env.has_unpolled_observations());
    assert!(!env.is_candidate_member());
    assert!(env.is_candidate_elder());
}

#[test]
#[ignore] //FIXME DKG is no longer carried out by parsec
fn when_accumulate_offline_and_start_dkg_and_section_info_then_node_is_removed_from_our_elders() {
    let mut env = Env::new(ELDER_SIZE - 1);
    let info1 = env.new_elders_info_with_candidate();
    env.accumulate_online(env.candidate.clone());
    env.accumulate_our_key_and_section_info_if_vote(&info1);
    env.accumulate_voted_unconsensused_events();

    let info2 = env.new_elders_info_without_candidate();

    env.other_ids = info1.new_other_ids;
    env.elders_info = info1.new_elders_info;
    env.public_key_set = info1.new_pk_set;

    env.accumulate_offline(*env.candidate.name());
    env.accumulate_our_key_and_section_info_if_vote(&info2);
    env.accumulate_voted_unconsensused_events();

    assert!(!env.has_unpolled_observations());
    assert!(!env.is_candidate_member());
    assert!(!env.is_candidate_elder());
}

#[test]
fn handle_bootstrap() {
    let mut env = Env::new(ELDER_SIZE);
    let new_node = OtherNode::new(&mut env.rng);

    let addr = *new_node.addr();
    let msg = new_node.bootstrap_request().unwrap();

    test_utils::handle_message(&mut env.subject, addr, msg).unwrap();
    env.poll();

    let response = new_node.expect_bootstrap_response();
    match response {
        BootstrapResponse::Join { elders_info, .. } => assert_eq!(elders_info, env.elders_info),
        BootstrapResponse::Rebootstrap(_) => panic!("Unexpected Rebootstrap response"),
    }
}

#[test]
#[ignore] //FIXME DKG is no longer carried out by parsec
fn send_genesis_update() {
    let mut env = Env::new(ELDER_SIZE);

    let old_section_key = *env.subject.section_key().expect("subject is not approved");

    let adult0 = env.gen_peer();
    let adult1 = env.gen_peer();

    env.accumulate_online(adult0.to_p2p_node());
    env.accumulate_online(adult1.to_p2p_node());

    // Remove one existing elder and promote an adult to take its place. This created new section
    // key.
    let dropped_name = *env.other_ids[0].0.public_id().name();
    env.perform_offline_and_promote(dropped_name, adult0.to_p2p_node());
    let new_section_key = *env.subject.section_key().expect("subject is not approved");

    // Create `GenesisUpdate` message and check its proof contains the previous key as well as the
    // new key.
    let message = utils::exactly_one(env.subject.create_genesis_updates());
    assert_eq!(message.0, adult1.to_p2p_node());

    let proof_chain = &message.1.proof_chain;
    assert!(proof_chain.has_key(&old_section_key));
    assert!(proof_chain.has_key(&new_section_key));
}

#[test]
fn handle_bounced_unknown_message() {
    let mut env = Env::new(ELDER_SIZE);

    // Ensure parsec version is > 0
    env.accumulate_parsec_prune();

    let dst = DstLocation::Section(env.rng.gen());
    let msg = env.accumulate_message(dst, Variant::UserMessage(b"unknown message".to_vec()));

    // Pretend that one of the other nodes is lagging behind and has not transitioned to elder yet
    // and so bounces a message to us as unknown.
    let other_node = env.create_transport_for_other_elder(0);
    let bounce_msg = Message::single_src(
        &env.other_ids[0].0,
        DstLocation::Direct,
        None,
        Variant::BouncedUnknownMessage {
            message: msg.to_bytes(),
            parsec_version: 0,
        },
    )
    .unwrap();

    test_utils::handle_message(&mut env.subject, *other_node.addr(), bounce_msg).unwrap();
    env.poll();

    let mut received_parsec_request = false;
    let mut received_resent_message = false;

    for (_, msg) in other_node.received_messages() {
        match msg.variant() {
            Variant::ParsecRequest(0, _) => received_parsec_request = true,
            Variant::UserMessage(_) => received_resent_message = true,
            _ => (),
        }
    }

    assert!(received_parsec_request);
    assert!(received_resent_message);
}

#[test]
#[ignore] //FIXME Any message invalidly signed will not deserialise / be created
fn handle_bounced_untrusted_message() {
    let mut env = Env::new(ELDER_SIZE);
    let old_section_key = *env.subject.section_key().expect("subject is not approved");

    // Simulate the section going through the elder change to generate new section key.
    let new = env.gen_peer().to_p2p_node();
    let old = *env.other_ids[0].0.public_id().name();
    env.accumulate_online(new.clone());
    env.perform_offline_and_promote(old, new);

    let new_section_key = *env.subject.section_key().expect("subject is not approved");

    let dst = DstLocation::Node(*env.other_ids[0].0.public_id().name());
    let msg = env.accumulate_message(dst, Variant::UserMessage(b"untrusted message".to_vec()));

    // Pretend that one of the other nodes is lagging behind and doesn't know the new key yet and
    // so bounces a message to us as untrusted.
    let other_node = env.create_transport_for_other_elder(0);
    let bounce_msg = Message::single_src(
        &env.other_ids[0].0,
        msg.src().src_location().to_dst(),
        Some(old_section_key),
        Variant::BouncedUntrustedMessage(Box::new(msg)),
    )
    .unwrap();

    test_utils::handle_message(&mut env.subject, *other_node.addr(), bounce_msg).unwrap();
    env.poll();

    let proof_chain = other_node
        .received_messages()
        .find_map(|(_, msg)| match (msg.variant(), msg.src()) {
            (Variant::UserMessage(_), SrcAuthority::Section { proof_chain, .. }) => {
                Some(proof_chain.clone())
            }
            _ => None,
        })
        .expect("message was not resent");

    assert!(proof_chain.has_key(&old_section_key)); // FIXME Why does this fail!
    assert!(proof_chain.has_key(&new_section_key));
}

#[test]
#[should_panic(expected = "FailedSignature")]
fn receive_message_with_invalid_signature() {
    let mut env = Env::new(ELDER_SIZE);

    let sk1 = consensus::test_utils::gen_secret_key(&mut env.rng);
    let pk1 = sk1.public_key();

    let (pk0, signature1) = env.sign_by_section(&bincode::serialize(&pk1).unwrap());
    let mut proof_chain = SectionProofChain::new(pk0);
    proof_chain.push(pk1, signature1);

    let src = SrcAuthority::Section {
        prefix: Prefix::default(),
        signature: sk1.sign(b"bad data"),
        proof_chain,
    };
    let msg = Message::unverified(
        src,
        DstLocation::Section(*env.subject.name()),
        Some(pk0),
        Variant::UserMessage(b"hello".to_vec()),
    )
    .unwrap();

    let sender = env.network.gen_addr();
    test_utils::handle_message(&mut env.subject, sender, msg).unwrap()
}

#[test]
#[should_panic(expected = "UntrustedMessage")]
fn receive_message_with_invalid_proof_chain() {
    let mut env = Env::new(ELDER_SIZE);

    let pk0 = *env.subject.section_key().expect("subject is not approved");
    let sk1 = consensus::test_utils::gen_secret_key(&mut env.rng);
    let pk1 = sk1.public_key();

    let invalid_sk0 = consensus::test_utils::gen_secret_key(&mut env.rng);
    let invalid_signature1 = invalid_sk0.sign(&bincode::serialize(&pk1).unwrap());

    let mut proof_chain = SectionProofChain::new(pk0);
    proof_chain.push_without_validation(pk1, invalid_signature1);

    let msg = PlainMessage {
        src: Prefix::default(),
        dst: DstLocation::Section(*env.subject.name()),
        dst_key: pk0,
        variant: Variant::UserMessage(b"hello".to_vec()),
    };

    let signature = sk1.sign(&bincode::serialize(&msg.as_signable()).unwrap());
    let src = SrcAuthority::Section {
        prefix: Prefix::default(),
        signature,
        proof_chain,
    };
    let msg = Message::unverified(src, msg.dst, Some(msg.dst_key), msg.variant).unwrap();

    let sender = env.network.gen_addr();
    test_utils::handle_message(&mut env.subject, sender, msg).unwrap();
}

struct OtherNode {
    transport: MockTransport,
    full_id: FullId,
}

impl OtherNode {
    fn new(rng: &mut MainRng) -> Self {
        Self {
            transport: MockTransport::new(None),
            full_id: FullId::gen(rng),
        }
    }

    fn addr(&self) -> &SocketAddr {
        self.transport.addr()
    }

    fn public_id(&self) -> &PublicId {
        self.full_id.public_id()
    }

    fn bootstrap_request(&self) -> Result<Message> {
        let variant = Variant::BootstrapRequest(*self.public_id().name());
        Ok(Message::single_src(
            &self.full_id,
            DstLocation::Direct,
            None,
            variant,
        )?)
    }

    fn received_messages(&self) -> impl Iterator<Item = Message> + '_ {
        self.transport.received_messages().map(|(_, msg)| msg)
    }

    fn expect_bootstrap_response(&self) -> BootstrapResponse {
        self.received_messages()
            .find_map(|msg| match msg.variant() {
                Variant::BootstrapResponse(response) => Some(response.clone()),
                _ => None,
            })
            .expect("BootstrapResponse not received")
    }
}
