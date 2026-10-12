import { renderToStaticMarkup } from 'react-dom/server'
import { expect, it } from 'vitest'
import { TranscriptOptions } from '../../src/components/transcript/TranscriptMenu.tsx'

it('keeps response copy available while incomplete history choices wait for the full transcript', () => {
	const html = renderToStaticMarkup(<TranscriptOptions historyReady={false} onSelect={() => {}} />)
	const buttons = html.match(/<button[\s\S]*?<\/button>/g) ?? []
	expect(buttons).toHaveLength(4)
	expect(buttons[0]).not.toMatch(/<button[^>]* disabled=/)
	for (const button of buttons.slice(1)) expect(button).toMatch(/<button[^>]* disabled=/)
})
