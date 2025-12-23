<p align="center">
  <h1 align="center">AgentFS</h1>
</p>

<p align="center">
  The filesystem for agents.
</p>

<p align="center">
  <a title="Build Status" target="_blank" href="https://github.com/tursodatabase/agentfs/actions/workflows/rust.yml"><img src="https://img.shields.io/github/actions/workflow/status/tursodatabase/agentfs/rust.yml?style=flat-square"></a>
  <a title="Rust" target="_blank" href="https://crates.io/crates/agentfs-sdk"><img alt="Crate" src="https://img.shields.io/crates/v/agentfs-sdk"></a>
  <a title="JavaScript" target="_blank" href="https://www.npmjs.com/package/agentfs-sdk"><img alt="NPM" src="https://img.shields.io/npm/v/agentfs-sdk"></a>
  <a title="Python" target="_blank" href="https://pypi.org/project/agentfs-sdk/"><img alt="PyPI" src="https://img.shields.io/pypi/v/agentfs-sdk"></a>
  <a title="MIT" target="_blank" href="https://github.com/tursodatabase/agentfs/blob/main/LICENSE.md"><img src="http://img.shields.io/badge/license-MIT-orange.svg?style=flat-square"></a>
</p>
<p align="center">
  <a title="Users's Discord" target="_blank" href="https://tur.so/discord"><img alt="Chat with other users of Turso (and Turso Cloud) on Discord" src="https://img.shields.io/discord/933071162680958986?label=Discord&logo=Discord&style=social&label=Users"></a>
</p>

---

> **⚠️ Warning:** This software is ALPHA; use only for development, testing, and experimentation. We are working to make it production-ready, but do not use it for critical data until it is ready.

## 🎯 What is AgentFS?

AgentFS is a filesystem explicitly designed for AI agents. Just as traditional filesystems provide file and directory abstractions for applications, AgentFS provides the storage abstractions that AI agents need.

The AgentFS repository consists of the following:

* **SDK** - [TypeScript](sdk/typescript), [Python](sdk/python), and [Rust](sdk/rust) libraries for programmatic filesystem access.
* **[CLI](MANUAL.md)** - Command-line interface for managing agent filesystems:
  - Mount AgentFS on host filesystem with FUSE on Linux and macFUSE on macOS.
  - Access AgentFS files with a command line tool.
* **[AgentFS Specification](SPEC.md)** - SQLite-based agent filesystem specification.

## 💡 Why AgentFS?

AgentFS provides the following benefits for agent state management:

* **Auditability**: Every file operation, tool call, and state change is recorded in a SQLite database file. Query your agent's complete history with SQL to debug issues, analyze behavior, or meet compliance requirements.
* **Reproducibility**: Snapshot an agent's state at any point with cp agent.db snapshot.db. Restore it later to reproduce exact execution states, test what-if scenarios, or roll back mistakes.
* **Portability**: The entire agent runtime—files, state, history —is stored in a single SQLite file. Move it between machines, check it into version control, or deploy it to any system where Turso runs.

Read more about the motivation for AgentFS in the announcement [blog post](https://turso.tech/blog/agentfs).

## 🧑‍💻 Getting Started

### Using the CLI

Initialize an agent filesystem:

```bash
$ agentfs init my-agent
Created agent filesystem: .agentfs/my-agent.db
Agent ID: my-agent
```

Inspect the agent filesystem:

```bash
$ agentfs fs ls my-agent
Using agent: my-agent
f hello.txt

$ agentfs fs cat my-agent hello.txt
hello from agent
```

You can also use a database path directly:

```bash
$ agentfs fs cat .agentfs/my-agent.db hello.txt
hello from agent
```

You can mount an agent filesystem using FUSE:

```bash
$ agentfs mount my-agent ./mnt
$ echo "hello" > ./mnt/hello.txt
$ cat ./mnt/hello.txt
hello
```

You can also run a program in an experimental sandbox with the agent filesystem mounted at `/agent`:

```bash
$ agentfs run /bin/bash
Welcome to AgentFS!

$ echo "hello from agent" > /agent/hello.txt
$ cat /agent/hello.txt
hello from agent
$ exit
```

Read the **[User Manual](MANUAL.md)** for complete documentation.

### Using the SDK

Install the SDK in your project:

```bash
npm install agentfs-sdk
```

Use it in your agent code:

```typescript
import { AgentFS } from 'agentfs-sdk';

// Persistent storage with identifier
const agent = await AgentFS.open({ id: 'my-agent' });
// Creates: .agentfs/my-agent.db

// Or use ephemeral in-memory database
const ephemeralAgent = await AgentFS.open();

// Key-value operations
await agent.kv.set('user:preferences', { theme: 'dark' });
const prefs = await agent.kv.get('user:preferences');

// Filesystem operations
await agent.fs.writeFile('/output/report.pdf', pdfBuffer);
const files = await agent.fs.readdir('/output');

// Tool call tracking
await agent.tools.record(
  'web_search',
  Date.now() / 1000,
  Date.now() / 1000 + 1.5,
  { query: 'AI' },
  { results: [...] }
);
```

### Examples

This source repository also contains examples that demonstrate how to integrate AgentFS with some popular AI frameworks:

- **[Mastra](examples/mastra/research-assistant)** - Research assistant using the Mastra AI framework
- **[Claude Agent SDK](examples/claude-agent/research-assistant)** - Research assistant using Anthropic's Claude Agent SDK
- **[OpenAI Agents](examples/openai-agents/research-assistant)** - Research assistant using OpenAI Agents SDK
- **[Firecracker](examples/firecracker)** - Minimal Firecracker VM with AgentFS mounted via NFSv3

See the **[examples](examples)** directory for more details.

## 🔧 How AgentFS Works?

<img align="right" width="40%" src=".github/assets/agentfs-arch.svg">

AgentFS is an agent filesystem accessible through an SDK that provides three essential interfaces for agent state management:

* **Filesystem:** A POSIX-like filesystem for files and directories
* **Key-Value:** A key-value store for agent state and context
* **Toolcall:** A toolcall audit trail for debugging and analysis

At the heart of AgentFS is the [agent filesystem](SPEC.md), a complete SQLite-based storage system for agents implemented using [Turso](https://github.com/tursodatabase/turso). Everything an agent does—every file it creates, every piece of state it stores, every tool it invokes—lives in a single SQLite database file.

## 🤔 FAQ

### How is AgentFS different from _X_?

[Bubblewrap](https://github.com/containers/bubblewrap) provides filesystem isolation using Linux namespaces and overlays. While you could achieve similar isolation with a `bwrap` call that mounts `/` read-only and uses `--tmp-overlay` on the working directory, the key difference is persistence and queryability: with AgentFS, the upper filesystem is stored in a single SQLite database file, which you can query, snapshot, and move to another machine. Read more about the motivation in the announcement [blog post](https://turso.tech/blog/agentfs).

[Docker Sandbox](https://www.docker.com/blog/docker-sandboxes-a-new-approach-for-coding-agent-safety/) and AgentFS are complementary rather than competing. AgentFS answers "what happened and what's the state?" while Docker Sandboxes answer "how do I run this safely?" You could use both together: run an agent inside a Docker Sandbox for security, while using AgentFS inside that sandbox for structured state management and audit trails.

## 📚 Learn More

- **[User Manual](MANUAL.md)** - Complete guide to using the AgentFS CLI and SDK
- **[Agent Filesystem Specification](SPEC.md)** - Technical specification of the agent filesystem SQLite schema
- **[SDK Examples](examples/)** - Working code examples using AgentFS
- **[Turso database](https://github.com/tursodatabase/turso)** - an in-process SQL database, compatible with SQLite.

### Blog Posts

- **[Introducing AgentFS](https://turso.tech/blog/agentfs)** - The motivation behind AgentFS
- **[AgentFS with FUSE](https://turso.tech/blog/agentfs-fuse)** - Mounting agent filesystems using FUSE
- **[Making Coding Agents Safe Using LlamaIndex](https://www.llamaindex.ai/blog/making-coding-agents-safe-using-llamaindex)** - Using AgentFS with LlamaIndex

## 📝 License

MIT
