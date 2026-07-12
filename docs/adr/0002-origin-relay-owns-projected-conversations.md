# Origin Relay owns projected Conversations

The Origin Relay is the sole authority for every Projected Shell Session's current Conversation and completed Turns; remote shells send only their identity, intent, current input, and host context. Each Projected Shell Session retains at most one current Conversation, and a new Conversation replaces it only after the first Turn commits successfully. Keeping projected Conversation state at the Origin Relay removes large history transport and duplicate retention logic, at the cost of making the relay stateful for the lifetime of its Projection Tree.
