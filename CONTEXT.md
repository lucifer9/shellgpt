# sgpt

sgpt projects locally configured AI assistance into local and remote shell sessions while keeping each shell's conversation context distinct.

## Language

**Shell Session**:
A local or projected interactive shell scope that owns one current Conversation. Each Shell Session has independent conversation context.
_Avoid_: Session

**Projected Shell Session**:
A Shell Session created on a remote host within a Projection Tree. Each Tunnel Hop creates a distinct Projected Shell Session.
_Avoid_: Remote session, tunnel session

**Origin Relay**:
The single relay at the root of a Projection Tree. It owns locally configured AI access and the temporary Conversations of every Projected Shell Session in that tree.
_Avoid_: Local relay, root relay

**Tunnel Hop**:
One SSH connection that extends a Projection Tree to another host without creating another relay.
_Avoid_: Relay hop, nested relay

**Projection Tree**:
One Origin Relay and all Projected Shell Sessions reached through its Tunnel Hops. Starting another Origin Relay creates a separate Projection Tree.
_Avoid_: Relay chain, tunnel chain

**Conversation**:
An ordered dialogue belonging to exactly one Shell Session. A Conversation consists of complete Turns and may be selected as that Shell Session's current Conversation.
_Avoid_: Chat, history, thread

**Turn**:
One user input paired with its successful assistant response within a Conversation.
_Avoid_: Message pair, exchange

**Input Anchor**:
A prior user input with non-empty stdin retained as reference context after its Turn leaves the recent Conversation window. It contains the original instruction and stdin payload, but not the corresponding assistant response.
_Avoid_: Anchor message, pinned Turn
