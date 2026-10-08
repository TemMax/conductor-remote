/** One visible, local Run choice from Conductor's effective settings. */
export interface DevRunConfig {
	id: string
	/** Conductor's display form: hyphens become spaces and words are capitalized. */
	name: string
	/** The command Conductor runs for this task. */
	command?: string
}
