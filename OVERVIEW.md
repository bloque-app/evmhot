# evmhot in plain words

**What it does.** Every person who can receive crypto through Bloque gets one deposit address from evmhot. The same
address works on Polygon and Base. When someone sends USDC (or another admitted token) to it, evmhot notices, moves
the money from that address to where it belongs, and tells the service that asked for the address: once when it sees
the deposit, once when the money has moved.

**What it is now.** Before, evmhot was a library that another Bloque service ran inside itself. From now on it is also a
service of its own: its own program, address on the network, database and deployment. The library stays, for anything
that wants to embed it.

**What it holds.** The keys of every deposit address, made from one secret phrase, and a second key that pays the
small gas fee each move costs. It also keeps a table that says which address belongs to which person and which
position in the secret phrase made it. Without that table the money in an address can't be moved, so it is backed up.

**Where the money goes.** Polygon USDC goes to bezent's reserve, where bezent turns it into bUSDC for the person. Other
tokens wait in their addresses until bezent can take them too. Each move carries the exact on-chain place (block and
position) where the money arrived, so bezent can check it, and who sent it, so a refund can go back to them.

**What changed from before.** Before, evmhot ran inside another service that has been shut down, and it stopped
seeing deposits when its data provider's monthly quota ran out, with no alarm. Now it runs on its own, alarms when it
falls behind or its provider refuses it, checks that each move actually succeeded, and only reads blocks that can no
longer change.

**How to read further.** `SPEC.md` is the exact behaviour, `PLAN.md` how it is built and run, `amendments/` every
change and why, `notes/RULINGS.md` the owner's decisions.
