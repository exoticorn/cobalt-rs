use std::cmp;
use std::collections::{VecDeque, BinaryHeap};
use shared::Config;

/// Maximum message ordering id before wrap around happens.
const MAX_ORDER_ID: u16 = 4096;

/// Number of bytes used in a single message header.
const MESSAGE_HEADER_BYTES: usize = 3;

/// Enum determing the way messages are send and received over connections.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum MessageKind {
    /// A message that is going be send exactly once and ingored in case its
    /// containing packet is lost. No guarantees are made as for the order in
    /// which a message of this kind is going to be received by a remote queue.
    Instant = 0,

    /// A message that is going to be re-send in case its containing packet is
    /// lost. No guarantees are made as for the order in which a message of
    /// this kind is going to be received by a remote queue.
    Reliable = 1,

    /// A message that is going to be re-send in case its containing packet is
    /// lost and is also guaranteed to arive in-order, meaning that if you send
    /// two `Ordered` messages and the second arrives first in the remote queue
    /// , the remote queue will buffer the second message until the first one
    /// arrives and then make both of them avaialable to the application at
    /// once.
    Ordered = 2,

    /// A invalid message which for some reason could not be parsed correctly
    /// from a packets data.
    Invalid = 3
}

/// A struct for handling messages inside a `MessageQueue` with support for
/// insertion into a binary min heap for order checking on received messages.
#[derive(Debug, Eq, PartialEq)]
struct Message {
    kind: MessageKind,
    order: u16,
    size: u8,
    data: Vec<u8>
}

impl Ord for Message {
    // Explicitly implement the trait so the queue becomes a min-heap
    // instead of a max-heap.
    fn cmp(&self, other: &Message) -> cmp::Ordering {
        other.order.cmp(&self.order)
    }
}

impl PartialOrd for Message {
    fn partial_cmp(&self, other: &Message) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A consuming iterator over the received messages in a `MessageQueue`.
pub struct MessageIterator<'a> {
    messages: &'a mut VecDeque<Message>
}

impl<'a> Iterator for MessageIterator<'a> {

    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.messages.pop_front() {
            Some(m) => Some(m.data),
            None => None
        }
    }

}

/// A queue that manages the sending and receival of both reliable and
/// unreliable message types and also supports in order transmission of
/// messages.
pub struct MessageQueue {

    /// The queue's configuration
    config: Config,

    /// The local order id which gets attached to all messages send as kind
    /// `MessageKind::Ordered`
    local_order_id: u16,

    /// The remote order id that is expected for the next incoming message of
    /// kind `MessageKind::Ordered`
    remote_order_id: u16,

    /// Queue of outgoing messages of the kind `MessageKind::Instant`
    i_queue: VecDeque<Message>,

    /// Queue of outgoing messages of the kind `MessageKind::Reliable`
    r_queue: VecDeque<Message>,

    /// Queue of outgoing messages of the kind `MessageKind::Ordered`
    o_queue: VecDeque<Message>,

    /// A ordered queue of incoming messages
    recv_queue: VecDeque<Message>,

    /// A binary min heap to manage incomging, out of order messages
    o_recv_heap: BinaryHeap<Message>
}

impl MessageQueue {

    /// Creates a new queue for sending and receiving messages.
    ///
    /// # Examples
    ///
    /// ```
    /// use cobalt::shared::{Config, MessageKind, MessageQueue};
    ///
    /// let mut q = MessageQueue::new(Config::default());
    /// q.send(MessageKind::Instant, b"I may get lost...".to_vec());
    /// q.send(MessageKind::Reliable, b"I'm getting there eventually".to_vec());
    /// q.send(MessageKind::Ordered, b"I'll always come before #2".to_vec());
    /// q.send(MessageKind::Ordered, b"#2".to_vec());
    /// ```
    pub fn new(config: Config) -> MessageQueue {
        MessageQueue {
            config: config,
            local_order_id: 0,
            remote_order_id: 0,
            i_queue: VecDeque::new(),
            r_queue: VecDeque::new(),
            o_queue: VecDeque::new(),
            recv_queue: VecDeque::new(),
            o_recv_heap: BinaryHeap::new()
        }
    }

    /// Returns a consuming iterator over all recevied messages in the queue.
    pub fn received(&mut self) -> MessageIterator {
        MessageIterator { messages: &mut self.recv_queue }
    }

    /// Pushes a message of the specified `kind` along with its `data` into the
    /// queue. The message will eventually get serialized via
    /// `MessageQueue::send_packet()`.
    pub fn send(&mut self, kind: MessageKind, data: Vec<u8>) {

        let message = Message {
            kind: kind,
            order: self.local_order_id,
            size: data.len() as u8,
            data: data
        };

        match kind {
            MessageKind::Instant => self.i_queue.push_back(message),
            MessageKind::Reliable => self.r_queue.push_back(message),
            MessageKind::Ordered => {

                self.o_queue.push_back(message);
                self.local_order_id += 1;

                if self.local_order_id == MAX_ORDER_ID {
                    self.local_order_id = 0;
                }

            },
            MessageKind::Invalid => {}
        }

    }

    /// Serializes a number of internally queued messages into the
    /// `available` space within the `packet`.
    ///
    /// The used algorithm first tries to fill the available space with the
    /// desired quotas for each `MessageKind` as defined in the queues
    /// configuration.
    ///
    /// Afterwards the remaining available space is filled by alternating
    /// between the different message kinds until there is finally no more
    /// space left to insert any further messages into the packet.
    ///
    /// For example, if we have `512` bytes available inside the packer and we
    /// specify that 60% of the packet data should be filled with
    /// `MessageKind::Instant` messages, then we will try to fill the
    /// buffer with at most `307` bytes of instant messages, at first.
    ///
    /// Then, after the other quotas have been taken into account, we'll try to
    /// fit more instant messages into the remaining available space within the
    /// packet.
    pub fn send_packet(&mut self, packet: &mut Vec<u8>, available: usize) {

        // First we are trying to fill the packet by using the set quotas
        let mut written = 0;
        write_messages(
            &mut self.i_queue, packet,
            (available as f32 / 100.0 * self.config.message_quota_instant) as usize,
            &mut written
        );

        write_messages(
            &mut self.r_queue, packet,
            (available as f32 / 100.0 * self.config.message_quota_reliable) as usize,
            &mut written
        );

        write_messages(
            &mut self.o_queue, packet,
            (available as f32 / 100.0 * self.config.message_quota_ordered) as usize,
            &mut written
        );

        // After that, we try to fill the remaining packet space by trying to
        // add one message of each kind until no more messages can be fit in
        let mut more = true;
        while more {
            more = false;
            more |= write_message(&mut self.i_queue, packet, available, &mut written);
            more |= write_message(&mut self.r_queue, packet, available, &mut written);
            more |= write_message(&mut self.o_queue, packet, available, &mut written);
        }

    }

    /// Parses the contents of a packet into messages, appending all valid
    /// messages into the internal receive queue.
    pub fn receive_packet(&mut self, packet: &[u8]) {
        for m in messages_from_packet(packet) {
            match m.kind {
                MessageKind::Instant => self.recv_queue.push_back(m),
                MessageKind::Reliable => self.recv_queue.push_back(m),
                MessageKind::Ordered => self.receive_ordered_message(m),
                MessageKind::Invalid => { /* ignore all other messages */ }
            }
        }
    }

    /// Parses the contents of a lost packet into messages, dropping all
    /// messages of the type `MessageKind::Instant` and prepending all
    /// remaining valid messages into the internal send queues for
    /// re-transmission.
    pub fn lost_packet(&mut self, packet: &[u8]) {
        for m in messages_from_packet(packet) {
            match m.kind {
                MessageKind::Instant => { /* ignore lost instant messages */ },
                MessageKind::Reliable => self.r_queue.push_front(m),
                MessageKind::Ordered => self.o_queue.push_front(m),
                MessageKind::Invalid => { /* ignore all other messages */ }
            }
        }
    }

    /// Resets the queue, clearing all its internal structures and order ids.
    pub fn reset(&mut self) {
        self.local_order_id = 0;
        self.remote_order_id = 0;
        self.i_queue.clear();
        self.r_queue.clear();
        self.o_queue.clear();
        self.recv_queue.clear();
        self.o_recv_heap.clear()
    }

    // Internal Message Handling ----------------------------------------------

    fn receive_ordered_message(&mut self, m: Message) {

        // Check if the order ID matches the currently expected on
        if m.order == self.remote_order_id {

            // Received the message in order
            self.recv_queue.push_back(m);
            self.remote_order_id += 1;

            if self.remote_order_id == MAX_ORDER_ID {
                self.remote_order_id = 0;
            }

            // Now check our heap for further messages we have received
            // out of order and check if they are next in the expected
            // order
            let mut matches = true;
            while matches {

                // Check if the order id of the minimal item in the heap
                // matches the expected next remote order id
                matches = if let Some(msg) = self.o_recv_heap.peek() {
                    msg.order == self.remote_order_id

                } else {
                    false
                };

                // We found another message, matching the next expected order id
                if matches {

                    // Remove it from the heap and push it into the recv queue
                    let msg = self.o_recv_heap.pop();
                    self.recv_queue.push_back(msg.unwrap());
                    self.remote_order_id += 1;

                    if self.remote_order_id == MAX_ORDER_ID {
                        self.remote_order_id = 0;
                    }

                }

            }

        // Otherwise check if the message order is more recent and if not, we
        // simply drop it. If it IS more recent, then we have received a future
        // message out of order.
        } else if order_is_more_recent(m.order, self.remote_order_id) {

            // Now before we insert the message into the min-heap, we check
            // that it's not already contained, in order to avoid duplicates.
            // TODO avoid duplication of inserts
            self.o_recv_heap.push(m);

        }

    }

}

// Static Helpers -------------------------------------------------------------
fn order_is_more_recent(a: u16, b: u16) -> bool {
    (a > b) && (a - b <= MAX_ORDER_ID / 2)
    || (b > a) && (b - a > MAX_ORDER_ID / 2)
}

fn messages_from_packet(packet: &[u8]) -> Vec<Message> {

    let available = packet.len();
    let mut index = 0;
    let mut messages = Vec::new();

    // Consume as long as message headers can be present
    while index < available && available - index >= 3 {

        // Upper 4 bits of kind are bits 9..11 of order
        let order_high = ((packet[index] & 0xF0) as u16) << 4;
        let order_low = packet[index + 1] as u16;

        // Byte 2 is the size
        let size = packet[index + 2] as usize;

        // Read available data
        messages.push(Message {

            // Lower 4 bits of byte 0 are the MessageKind
            kind: match packet[index] & 0x0F {
                0 => MessageKind::Instant,
                1 => MessageKind::Reliable,
                2 => MessageKind::Ordered,
                _ => MessageKind::Invalid
            },

            order: order_high | order_low,
            size: size as u8,
            data: packet[
                index + MESSAGE_HEADER_BYTES..cmp::min(
                    index + MESSAGE_HEADER_BYTES + size,
                    available
                )
            ].to_vec()

        });

        index += size + MESSAGE_HEADER_BYTES;

    }

    messages

}

fn write_messages(
    queue: &mut VecDeque<Message>,
    packet: &mut Vec<u8>,
    available: usize,
    written: &mut usize
) {
    let mut used = 0;
    while write_message(queue, packet, available, &mut used) {}
    *written += used;
}

fn write_message(
    queue: &mut VecDeque<Message>,
    packet: &mut Vec<u8>,
    available: usize,
    written: &mut usize
) -> bool {

    if queue.is_empty() == false {

        let required = {
            (queue.front().unwrap().size as usize) + MESSAGE_HEADER_BYTES
        };

        // If adding this message would exceed the available bytes, exit
        if required > available - *written {
            false

        // Remove and serialize the message into the packet
        } else {
            let message = queue.pop_front().unwrap();
            packet.push(
                ((message.order & 0x0F00) >> 4) as u8 | (message.kind as u8)
            );
            packet.push(message.order as u8);
            packet.push(message.size);
            packet.extend(message.data.iter().cloned());
            *written += required;
            true
        }

    } else {
        false
    }

}

#[cfg(test)]
mod tests {

    use shared::Config;
    use super::{MessageKind, MessageQueue};

    fn messages(q: &mut MessageQueue) -> Vec<Vec<u8>> {
        let mut messages = Vec::new();
        for m in q.received() {
            messages.push(m);
        }
        messages
    }

    #[test]
    fn test_send_write() {

        let mut q = MessageQueue::new(Config::default());

        // Filled from quota
        q.send(MessageKind::Instant, b"Hello World".to_vec());
        q.send(MessageKind::Instant, b"Hello World".to_vec());

        // Added by filling buffer
        q.send(MessageKind::Instant, b"Hello World".to_vec());

        // Put into packet 2
        q.send(MessageKind::Instant, b"Hello World2".to_vec());
        q.send(MessageKind::Instant, b"Hello World2".to_vec());

        // Filled from quota
        q.send(MessageKind::Reliable, b"Foo".to_vec());

        // Put into packet 2 by quota
        q.send(MessageKind::Reliable, b"Foo2".to_vec());

        // Put into packet 2 by filling buffer
        q.send(MessageKind::Reliable, b"Foo More".to_vec());

        // Filled from quota
        q.send(MessageKind::Ordered, b"Bar".to_vec());

        // Put into packet 2 by quota
        q.send(MessageKind::Ordered, b"Bar2".to_vec());

        // Put into packet 3
        q.send(MessageKind::Ordered, b"Bar More".to_vec());
        q.send(MessageKind::Ordered, b"Bar Even More".to_vec());

        // Check Packet 1
        let mut buffer = Vec::new();
        q.send_packet(&mut buffer, 60);

        assert_eq!(buffer, [
            // Hello World
            0, 0, 11, 72, 101, 108, 108, 111, 32, 87, 111, 114, 108, 100,
            // Hello World
            0, 0, 11, 72, 101, 108, 108, 111, 32, 87, 111, 114, 108, 100,
            // Foo
            1, 0, 3, 70, 111, 111,
            // Bar
            2, 0, 3, 66, 97, 114,
            // Hello World
            0, 0, 11, 72, 101, 108, 108, 111, 32, 87, 111, 114, 108, 100

        ].to_vec());

        // Check Packet 2
        let mut buffer = Vec::new();
        q.send_packet(&mut buffer, 64);

        assert_eq!(buffer, [
            // Hello World2
            0, 0, 12, 72, 101, 108, 108, 111, 32, 87, 111, 114, 108, 100, 50,
            // Hello World2
            0, 0, 12, 72, 101, 108, 108, 111, 32, 87, 111, 114, 108, 100, 50,
            // Foo2
            1, 0, 4, 70, 111, 111, 50,
            // Bar2
            2, 1, 4, 66, 97, 114, 50,
            // Foo More
            1, 0, 8, 70, 111, 111, 32, 77, 111, 114, 101

        ].to_vec());

        // Check Packet 3
        let mut buffer = Vec::new();
        q.send_packet(&mut buffer, 64);

        assert_eq!(buffer, [
            // Bar More
            2, 2, 8, 66, 97, 114, 32, 77, 111, 114, 101,

            // Bar Even More
            2, 3, 13, 66, 97, 114, 32, 69, 118, 101, 110, 32, 77, 111, 114, 101
        ].to_vec());

    }

    #[test]
    fn test_packet_lost_write() {

        let mut q = MessageQueue::new(Config::default());

        q.lost_packet(&[
            // Hello World2
            0, 0, 12, 72, 101, 108, 108, 111, 32, 87, 111, 114, 108, 100, 50,
            // Hello World2
            0, 0, 12, 72, 101, 108, 108, 111, 32, 87, 111, 114, 108, 100, 50,
            // Foo2
            1, 0, 4, 70, 111, 111, 50,
            // Bar2
            2, 1, 4, 66, 97, 114, 50,
            // Foo More
            1, 0, 8, 70, 111, 111, 32, 77, 111, 114, 101
        ]);

        // Send some more messages
        q.send(MessageKind::Instant, b"Hello World".to_vec());
        q.send(MessageKind::Reliable, b"Foo5".to_vec());
        q.send(MessageKind::Ordered, b"Bar3".to_vec());

        let mut buffer = Vec::new();
        q.send_packet(&mut buffer, 64);
        assert_eq!(buffer, [

            // Hello World
            0, 0, 11, 72, 101, 108, 108, 111, 32, 87, 111, 114, 108, 100,

            // Foo More
            1, 0, 8, 70, 111, 111, 32, 77, 111, 114, 101,

            // Bar2
            2, 1, 4, 66, 97, 114, 50,

            // Foo2
            1, 0, 4, 70, 111, 111, 50,

            // Bar3
            2, 0, 4, 66, 97, 114, 51,

            // Foo5
            1, 0, 4, 70, 111, 111, 53

        ].to_vec());

    }

    #[test]
    fn test_reset() {

        let mut q = MessageQueue::new(Config::default());
        q.send(MessageKind::Instant, b"Hello World".to_vec());
        q.send(MessageKind::Instant, b"Hello World".to_vec());
        q.send(MessageKind::Reliable, b"Hello World".to_vec());
        q.send(MessageKind::Ordered, b"Hello World".to_vec());
        q.send(MessageKind::Ordered, b"Hello World".to_vec());

        // Reset all queues and order ids
        q.reset();

        // Check that nothing gets serialized
        let mut buffer = Vec::new();
        q.send_packet(&mut buffer, 64);
        assert_eq!(buffer, [].to_vec());

        // Check that local_order_id has been reset
        q.send(MessageKind::Ordered, b"".to_vec());
        q.send_packet(&mut buffer, 64);
        assert_eq!(buffer, [2, 0, 0].to_vec());
    }

    #[test]
    fn test_receive_read() {

        let mut q = MessageQueue::new(Config::default());
        let packet = [
            // Hello World
            0, 0, 11, 72, 101, 108, 108, 111, 32, 87, 111, 114, 108, 100,
            // Hello World
            0, 0, 11, 72, 101, 108, 108, 111, 32, 87, 111, 114, 108, 100,
            // Foo
            1, 0, 3, 70, 111, 111,
            // Bar
            2, 0, 3, 66, 97, 114,
            // Hello World
            0, 0, 11, 72, 101, 108, 108, 111, 32, 87, 111, 114, 108, 100
        ].to_vec();

        q.receive_packet(&packet[..]);

        assert_eq!(messages(&mut q), [
            b"Hello World".to_vec(),
            b"Hello World".to_vec(),
            b"Foo".to_vec(),
            b"Bar".to_vec(),
            b"Hello World".to_vec()
        ]);

    }

    #[test]
    fn test_receive_read_out_of_order() {

        let mut q = MessageQueue::new(Config::default());

        // Receive one out of order(#1) "World" message
        q.receive_packet(&[
            2, 1, 5, 87, 111, 114, 108, 100
        ]);

        // We expect no message yet
        assert!(messages(&mut q).is_empty());

        // Receive one out of order(#3) "order!" message
        q.receive_packet(&[
            2, 3, 6, 111, 114, 100, 101, 114, 33
        ]);

        // We still expect no message yet
        assert!(messages(&mut q).is_empty());

        // Receive the actual first "Hello" message
        q.receive_packet(&[
            2, 0, 5, 72, 101, 108, 108, 111
        ]);

        // We now expect both "Hello" and "World"
        assert_eq!(messages(&mut q), [b"Hello", b"World"]);

        // Receive the order(#2) "out of" message
        q.receive_packet(&[
            2, 2, 6, 111, 117, 116, 32, 111, 102
        ]);

        // We now expect both "out of" and "order!"
        assert_eq!(messages(&mut q), [b"out of", b"order!"]);
    }

    #[test]
    fn test_receive_empty() {

        let mut q = MessageQueue::new(Config::default());

        // Receive 2 empty messages
        q.receive_packet(&[
            0, 0, 0,
            0, 0, 0
        ]);

        assert_eq!(messages(&mut q), [b"", b""]);

    }

    #[test]
    fn test_receive_invalid() {

        let mut q = MessageQueue::new(Config::default());

        // Receive a message with a invalid kind
        q.receive_packet(&[
            255, 0, 0
        ]);

        assert!(messages(&mut q).is_empty());

        // Receive a message with incomplete header
        q.receive_packet(&[
            0, 0
        ]);

        // Receive a message with incomplete data
        q.receive_packet(&[
            0, 0, 15, 72, 101, 108, 108, 111 // 15 bytes but only 5 in buffer
        ]);

        assert_eq!(messages(&mut q), [b"Hello"]);

    }

    #[test]
    fn test_receive_ordered_decoding_wrap_around() {

        let mut q = MessageQueue::new(Config::default());
        for i in 0..4096 {

            q.receive_packet(&[
                2 | ((i & 0x0F00) >> 4) as u8, (i as u8), 2, (i >> 8) as u8, i as u8
            ]);

            assert_eq!(messages(&mut q), [[(i >> 8) as u8, i as u8]]);

        }

        // Should now expect order=0 again
        q.receive_packet(&[
            2, 0, 2, 0, 0
        ]);
        assert_eq!(messages(&mut q), [[0, 0]]);

    }

    #[test]
    fn test_receive_ordered_encoding_wrap_around() {

        let mut q = MessageQueue::new(Config::default());
        for i in 0..4096 {

            q.send(MessageKind::Ordered, [(i >> 8) as u8, i as u8].to_vec());

            let mut buffer = Vec::new();
            q.send_packet(&mut buffer, 64);
            assert_eq!(buffer, [
                2 | ((i & 0x0F00) >> 4) as u8, (i as u8), 2, (i >> 8) as u8, i as u8].to_vec()
            );

        }

        // Should now write order=0 again
        q.send(MessageKind::Ordered, [0, 0].to_vec());

        let mut buffer = Vec::new();
        q.send_packet(&mut buffer, 64);
        assert_eq!(buffer, [2, 0, 2, 0, 0].to_vec());

    }

}
