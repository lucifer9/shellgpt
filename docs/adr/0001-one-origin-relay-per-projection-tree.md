# Use one Origin Relay per Projection Tree

Each root `sgpt tunnel ssh` starts one Origin Relay, while nested SSH connections extend the same Projection Tree as Tunnel Hops and never create or federate additional relays. This preserves the remote zero-binary model and avoids relay-to-relay state transfer, authentication, and failure semantics while still allowing nested and branched projections within the configured Projected Shell Session limit.
