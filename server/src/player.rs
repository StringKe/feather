//! This module provides systems and components
//! relating to players, including player movement
//! and inventory handling.

use crate::chunkclient::{ChunkLoadEvent, ChunkWorkerHandle};
use crate::entity::{broadcast_entity_movement, EntityComponent, PlayerComponent};
use crate::network::{send_packet_to_player, NetworkComponent, PacketQueue};
use feather_core::network::cast_packet;
use feather_core::network::packet::implementation::{
    ChunkData, PlayerLook, PlayerPosition, PlayerPositionAndLookServerbound,
};
use feather_core::network::packet::{Packet, PacketType};
use feather_core::world::chunk::Chunk;
use feather_core::world::{ChunkMap, ChunkPosition, Position};
use hashbrown::HashSet;
use rayon::prelude::*;
use shrev::EventChannel;
use specs::storage::BTreeStorage;
use specs::{
    Component, Entities, Entity, LazyUpdate, ParJoin, Read, ReadStorage, ReaderId, System, World,
    WorldExt, WriteStorage,
};
use std::ops::{Deref, DerefMut};

/// System for handling player movement
/// packets.
pub struct PlayerMovementSystem;

impl<'a> System<'a> for PlayerMovementSystem {
    type SystemData = (
        WriteStorage<'a, EntityComponent>,
        ReadStorage<'a, PlayerComponent>,
        Read<'a, PacketQueue>,
        ReadStorage<'a, NetworkComponent>,
        Entities<'a>,
        Read<'a, LazyUpdate>,
    );

    fn run(&mut self, data: Self::SystemData) {
        let (mut ecomps, pcomps, packet_queue, netcomps, entities, _) = data;

        // Take movement packets
        let mut packets = vec![];
        packets.append(&mut packet_queue.for_packet(PacketType::PlayerPosition));
        packets.append(&mut packet_queue.for_packet(PacketType::PlayerPositionAndLookServerbound));
        packets.append(&mut packet_queue.for_packet(PacketType::PlayerLook));

        // Handle movement packets
        for (player, packet) in packets {
            let ecomp = ecomps.get(player).unwrap();

            // Get position using packet and old position
            let (new_pos, has_moved, has_looked) = new_pos_from_packet(ecomp.position, packet);

            // Broadcast position update
            broadcast_entity_movement(
                player,
                ecomp.position,
                new_pos,
                has_moved,
                has_looked,
                &netcomps,
                &pcomps,
                &entities,
            );

            // Set new position
            ecomps.get_mut(player).unwrap().position = new_pos;
        }
    }
}

fn new_pos_from_packet(old_pos: Position, packet: Box<Packet>) -> (Position, bool, bool) {
    let mut has_looked = false;
    let mut has_moved = false;

    let pos = match packet.ty() {
        PacketType::PlayerPosition => {
            has_moved = true;
            let packet = cast_packet::<PlayerPosition>(&packet);

            Position::new(
                packet.x,
                packet.feet_y,
                packet.z,
                old_pos.pitch,
                old_pos.yaw,
            )
        }
        PacketType::PlayerLook => {
            has_looked = true;
            let packet = cast_packet::<PlayerLook>(&packet);

            Position::new(old_pos.x, old_pos.y, old_pos.z, packet.pitch, packet.yaw)
        }
        PacketType::PlayerPositionAndLookServerbound => {
            has_moved = true;
            has_looked = true;
            let packet = cast_packet::<PlayerPositionAndLookServerbound>(&packet);

            Position::new(packet.x, packet.feet_y, packet.z, packet.pitch, packet.yaw)
        }
        _ => panic!(),
    };

    (pos, has_moved, has_looked)
}

/// Component storing what chunks are pending
/// to send to a player.
#[derive(Clone, Debug)]
pub struct ChunkPendingComponent {
    pub pending: HashSet<ChunkPosition>,
}

impl Deref for ChunkPendingComponent {
    type Target = HashSet<ChunkPosition>;

    fn deref(&self) -> &Self::Target {
        &self.pending
    }
}

impl DerefMut for ChunkPendingComponent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.pending
    }
}

impl Component for ChunkPendingComponent {
    type Storage = BTreeStorage<Self>;
}

/// System for sending chunks to players once they're loaded.
///
/// This system listens to `ChunkLoadEvent`s.
pub struct ChunkSendSystem {
    load_event_reader: Option<ReaderId<ChunkLoadEvent>>,
}

impl ChunkSendSystem {
    pub fn new() -> Self {
        Self {
            load_event_reader: None,
        }
    }
}

impl<'a> System<'a> for ChunkSendSystem {
    type SystemData = (
        WriteStorage<'a, ChunkPendingComponent>,
        ReadStorage<'a, NetworkComponent>,
        Read<'a, ChunkMap>,
        Read<'a, EventChannel<ChunkLoadEvent>>,
    );

    fn run(&mut self, data: Self::SystemData) {
        let (mut pendings, netcomps, chunk_map, load_events) = data;

        for event in load_events.read(&mut self.load_event_reader.as_mut().unwrap()) {
            // TODO perhaps this is slightly inefficient?
            (&netcomps, &mut pendings)
                .par_join()
                .for_each(|(net, pending)| {
                    if pending.contains(&event.pos) {
                        // It's safe to unwrap the chunk value now,
                        // because we know it's been loaded.
                        let chunk = chunk_map.chunk_at(event.pos).unwrap();
                        send_chunk_data(chunk, net);

                        pending.remove(&event.pos);
                    }
                });
        }
    }

    fn setup(&mut self, world: &mut World) {
        use specs::SystemData;
        Self::SystemData::setup(world);
        self.load_event_reader = Some(
            world
                .fetch_mut::<EventChannel<ChunkLoadEvent>>()
                .register_reader(),
        );
    }
}

fn send_chunk_data(chunk: &Chunk, net: &NetworkComponent) {
    let packet = ChunkData::new(chunk.clone());
    send_packet_to_player(net, packet);
}

/// Attempts to send the chunk at the given position to
/// the given player. If the chunk is not loaded, it will
/// be loaded and sent at a later time as soon as it is
/// loaded.
pub fn send_chunk_to_player(
    chunk_pos: ChunkPosition,
    net: &NetworkComponent,
    player: Entity,
    chunk_map: &ChunkMap,
    chunk_handle: &ChunkWorkerHandle,
    lazy: &LazyUpdate,
) {
    if let Some(chunk) = chunk_map.chunk_at(chunk_pos) {
        send_chunk_data(chunk, net);
    } else {
        // Queue for loading
        lazy.exec_mut(move |world| {
            world
                .write_component::<ChunkPendingComponent>()
                .get_mut(player)
                .unwrap()
                .pending
                .insert(chunk_pos);
        });
    }
}
