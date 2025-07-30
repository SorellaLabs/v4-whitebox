# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Uni-V4 repo is our tools for loading and maintaining unswap v4 pool state in order for intergrators to use this to model transactions on Angstrom

## Key Commands
```bash
cargo build
cargo +nightly fmt
cargo clippy 
```

## Architecture (`/src`)
- **uni_structure**: api of managed pool state for users to generate swaps against
- **uniswap**: core pool loading + tracking of pool state

### Key Design Decisions
1. **BaselinePoolFactory** Does all loading of state. This includes fetching more ticks when needed to ensure that our tick range deadband is correct.
2. **PoolManagerService**  Handles consuming updates from on-chain applying them to the proper pools as needed aswell as monitoring if we need to fetch more ticks via factory.

