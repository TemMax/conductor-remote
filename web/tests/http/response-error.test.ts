import { describe, expect, test } from 'vitest'
import { responseErrorMessage } from '../../src/contract/shared.ts'

describe('HTTP response errors', () => {
	test('keeps structured HTTP errors readable to web and MCP clients', () => {
		expect(responseErrorMessage({ code: 'model_missing', message: 'Pick an exact model.' }, 'HTTP 409')).toBe(
			'Pick an exact model.'
		)
		expect(responseErrorMessage('plain failure', 'HTTP 500')).toBe('plain failure')
		expect(responseErrorMessage({}, 'HTTP 500')).toBe('HTTP 500')
	})
})
