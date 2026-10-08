import type { DevRunConfig } from './run-configs.ts'

export interface DevServerState {
	/** The local Tailscale prerequisites needed to expose a Run task are ready. */
	available: boolean
	/** Something currently accepts TCP connections on the workspace's primary port. */
	running: boolean
	/** This relay owns an active tailnet-only Tailscale Serve mapping. */
	forwarded: boolean
	/** The workspace's primary `CONDUCTOR_PORT`, when it can be discovered. */
	port: number | null
	/** Tailnet-only HTTPS URL, present only while forwarded. */
	url: string | null
	/** Configured or detected previews, in Conductor's Open-menu order. */
	forwards: DevServerForward[]
	/** Named Run choices visible in Conductor for this local workspace. */
	runConfigs: DevRunConfig[]
	/** The selected Conductor Run task, known after a start/stop action. */
	task?: string
	/** Why this workspace cannot currently be controlled or forwarded. */
	error?: string
}

export interface DevServerForward {
	/** Configured preview name; detected/fallback ports use "Port N". */
	name: string
	/** Local loopback port behind this forward. */
	port: number
	/** Something currently accepts TCP connections on `port`. */
	running: boolean
	/** This relay owns and can verify the Tailscale mapping. */
	forwarded: boolean
	/** Tailnet-only URL, including the configured path, when forwarded. */
	url: string | null
}

export interface DevServerResult extends DevServerState {
	ok: boolean
	/** Conductor's button actually changed, rather than already matching the request. */
	changed?: boolean
}
