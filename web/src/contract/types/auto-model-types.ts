import type { AgentEffort } from '../wire.ts'

export interface AutoModelTuple {
	model: string
	effort?: AgentEffort
	fast?: boolean
}

export interface AutoModelProfile extends AutoModelTuple {
	id: string
	description: string
}

export interface AutoModelConfig {
	version: 1
	defaultAuto: boolean
	router: AutoModelTuple
	profiles: AutoModelProfile[]
	fallback: string
	rules: string
	timeoutMs: number
}

export interface AutoModelDecision extends AutoModelTuple {
	profile: string
	reason: string
	fallback: boolean
	durationMs: number
}

export interface AutoModelState {
	status: 'draft' | 'selecting' | 'waiting' | 'failed' | 'delivered' | 'cancelled'
	decision?: AutoModelDecision
	error?: string
}

export interface AutoModelConfigResponse {
	config: AutoModelConfig
	issues: string[]
}
