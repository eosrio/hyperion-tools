# Hyperion serving

The language used to define shared serving for Hyperion and its supported API families.

## Language

**ABI deployment**:
An on-chain publication of a contract's ABI at a particular chain position. Contract mappings are
bound to the ABI deployment applicable to the data they interpret.

**API domain**:
A coherent query surface for one area of chain functionality, such as AtomicAssets or Light API.
All enabled state-query domains on a chain use that chain's selected state backend.

**API domain plugin**:
An extension that registers a domain's API routes and serves them through shared domain/query
interfaces. It may use contract-indexing definitions/hooks but is distinct from protocol adaptation
and storage-backend implementation. Domain lifecycle semantics, such as AtomicAssets burn handling,
belong to its plugin definitions/hooks and query behavior.

**Trusted executable plugin**:
Plugin code the operator chooses to install and use, taking responsibility for auditing it. Trust
in that code is distinct from trusting indexed contract data or confining code in a security sandbox.

**Adapter extension**:
An independently versioned package implementing a backend adapter interface. Its package version is
separate from the database engine version.

**Backend adapter**:
An implementation of the shared storage/query and capability contracts using a backend's native
operations, preserving the declared behavior while choosing backend-specific physical execution.
It implements rollback of stored projection effects under shared-runtime fork-recovery coordination.

**AtomicAssets plugin**:
The EOS Rio-maintained plugin package containing AtomicAssets and AtomicMarket indexing and APIs.
It has one operator-facing version and release, with modular internal implementation.

**Atomic group**:
A declared group of projection mutations that must commit all-or-nothing within the selected state
backend. Definitions/plugins require this capability explicitly; ordinary mutation batches do not
imply it.

**Chain protocol adapter**:
An extension that interprets a chain's binary, ABI, timestamp, delta, and key conventions as common
events while preserving protocol-specific information.

**Chain extension package**:
One operator-managed, versioned package that can supply protocol profiles, codec providers, and
optional contract/API modules through separate interfaces. Tooling coordinates the required
Node/Rust components without requiring the operator to manage them separately.

**Full auto-map mode**:
State indexing in which Hyperion generates and maintains contract mappings from the ABIs of accounts
allowed by the operator, including when those accounts deploy ABI updates.

**Contract indexing plugin**:
An extension that consumes common chain events and defines a contract's state/history projections,
query fields, indexes, and specialized processing.

**Contract indexing definition**:
A declarative description of the chain events, projections, query fields, and indexes needed for a
contract, authored or generated in full auto-map mode. The same definition applies to either supported
state backend and may refer to code hooks for complex behavior.

**Compatible plugin set**:
The tested, pinned plugin versions identified for a Hyperion release, giving operators a supported
core/plugin combination without manual version matching.

**SDK contract version**:
The independently versioned shared interfaces and semantics targeted by a plugin. Changes remain
backward compatible within a major version; breaking contract changes require a new major.

**Official plugin catalog**:
The EOS Rio-provided repository that lists plugins and declares compatibility. Catalog metadata
informs plugin selection; it does not initiate executable code upgrades or replace operator auditing.

**Idempotent processing**:
Applying the same source event again preserves the intended projection result without duplicating
effects, double-counting derived values, or regressing state.

**Mapping revision**:
An immutable revision of a contract indexing definition within its ABI-deployment binding. It allows
off-chain indexing changes without a new on-chain ABI deployment.

**Live mapping reload**:
Adding or updating declarative contract mappings without restarting the indexer, with validation and
staged rebuild/activation when required. This is mandatory for the 4.5 launch; hooks and native
protocol components may still use controlled restarts.

**Logical query**:
A structured backend-neutral description of requested reads and their semantics. Plugins compose
logical queries; backend adapters translate and optimize their execution through native operations.

**Mutation batch**:
A batch of backend-neutral projection changes produced or staged by declarative mappings/code
hooks, validated and applied through the core-managed backend contract. It does not automatically
constitute a multi-record transaction; atomic groups are declared separately.

**Projection hook**:
Executable plugin logic that reads through shared interfaces and computes/stages projection changes
for runtime-managed application. The 4.5 default is JavaScript execution with TypeScript authoring
through the shared SDK; protocol codecs are separate components.

**Replay determinism**:
Equivalent logical projection effects from the same versioned source inputs, applicable processing
revisions, and declared prior state, independent of when the inputs are processed.

**Projection partition**:
A declared independent ordering scope within a projection. Events remain ordered inside it while
independent partitions may execute concurrently.

**Projection initialization requirements**:
The supported ways to establish a projection's initial state and the source data and coverage needed
to do so. A snapshot followed by catch-up and a backfill of earlier history satisfy different needs;
a projection is ready only after its declared requirements are met.

**Projection initialization job**:
Shared-runtime work that establishes or rebuilds a projection from declared inputs, tracking durable
progress, retry/resume, and readiness. The plugin supplies requirements and domain transformations.

**Recovery window**:
The configurable bounded extent of locally retained inputs for recovering an isolated projection
failure. Older recovery inputs may come from historical sources; unavailable required inputs leave
the projection incomplete without stopping unrelated indexing.

**Plugin dependency**:
A declared projection/query prerequisite and compatible version required by a plugin. Enabling
the plugin activates required indexing dependencies within configured contract scope; it does
not automatically expose another plugin's API routes.

**Parity baseline**:
The pinned reference API source revision that defines the client-visible behavior to reproduce
for the API domains in scope.

**Last irreversible block (LIB)**:
The chain's reported finality boundary. Blocks after it remain reversible.

**Reversible head state**:
The latest successfully processed state on the canonical branch, including changes after LIB.
Its reversible changes may be replaced when the chain forks.

**State read context**:
The selected backend and compatible projection versions bound to a state request. This version
binding is separate from the endpoint's declared database-snapshot consistency.

**Read snapshot**:
A stable database view shared by related reads. Plugins declare this requirement per endpoint;
it does not by itself synchronize chain progress across projections, state, and history.

**Last valid view**:
A preserved, compatible view that a plugin supports serving after an isolated projection failure.
Its responses identify the block through which the served projection scope is complete; that
progress differs from chain head and individual row modification blocks. HTTP plugins expose the
block marker and last-valid-state indicator through shared headers while preserving body compatibility.
Serving this view after a failure requires explicit plugin support; affected queries otherwise return
a retryable unavailable error, while unrelated queries and indexing continue.

**Backend cutover**:
The operator-initiated switch of a chain's active state backend to an initialized, caught-up, and
validated destination. All enabled state domains use the selected destination after the switch.

**State backend**:
The database implementation used to store and query the current chain state for Hyperion's state
query surface.

**History-preserving upgrade**:
An upgrade that keeps existing indexed history usable and resumes indexing without reprocessing
the complete chain history. New or changed state projections initialize or rebuild separately.

**State query**:
A query whose subject is the current chain state, such as an asset's ownership or an account's token
balance. The finality and visibility of that state are defined separately from the term itself.
