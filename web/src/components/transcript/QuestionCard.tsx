import { useRef, useState } from 'react'
import { ApiError, client } from '../../lib/api.ts'
import type { QuestionAnswer, QuestionRequest } from '../../lib/types.ts'

export function QuestionCard({
	request,
	sessionId,
	workspaceId,
	active,
	output
}: {
	request: QuestionRequest
	sessionId: string | null
	workspaceId: string
	active: boolean
	output?: string
}) {
	const [answers, setAnswers] = useState<QuestionAnswer[]>(() =>
		request.questions.map(() => ({ selected: [], other: '' }))
	)
	const [sending, setSending] = useState(false)
	const [submitted, setSubmitted] = useState(false)
	const [unconfirmed, setUnconfirmed] = useState(false)
	const [error, setError] = useState<string | null>(null)
	const inFlight = useRef(false)
	const editable = active && !submitted && !sending && !!sessionId
	const complete = answers.every(a => a.selected.length > 0 || !!a.other?.trim())
	const change = (index: number, answer: QuestionAnswer) =>
		setAnswers(prev => prev.map((a, i) => (i === index ? answer : a)))
	const send = async () => {
		if (!editable || !complete || !sessionId || inFlight.current) return
		inFlight.current = true
		setSending(true)
		setError(null)
		try {
			const result = await client.answerQuestions(sessionId, workspaceId, request.id, answers)
			if (result.answers) setAnswers(result.answers)
			setSubmitted(true)
		} catch (err) {
			if (err instanceof ApiError && err.submitted) {
				setSubmitted(true)
				setUnconfirmed(true)
			}
			setError(err instanceof Error ? err.message : String(err))
		} finally {
			inFlight.current = false
			setSending(false)
		}
	}
	return (
		<section aria-label="Agent question" className="min-w-0 rounded-xl border border-border-soft bg-surface p-3">
			<div className="mb-3 flex justify-between gap-2 text-xs text-muted">
				<span>{request.provider === 'claude' ? 'Claude' : 'Codex'} · Question</span>
				<span>
					{sending
						? 'Sending…'
						: unconfirmed
							? 'Submitted · unconfirmed'
							: submitted || output
								? 'Answered'
								: active
									? 'Awaiting your answer'
									: 'Closed'}
				</span>
			</div>
			<div className="flex flex-col gap-4">
				{request.questions.map((question, i) => (
					<fieldset key={question.question} disabled={!editable} className="min-w-0">
						<legend className="mb-2 text-sm font-medium text-text">
							{question.header ? <span className="mr-2 text-muted">{question.header}</span> : null}
							{question.question}
						</legend>
						{question.multiSelect && editable ? (
							<p className="mb-2 text-xs text-muted">Select all that apply.</p>
						) : null}
						<div className="flex flex-col gap-2">
							{question.options.map((option, j) => (
								<label
									key={JSON.stringify(option)}
									className="flex items-start gap-2 rounded-lg border border-border-soft px-3 py-2 text-sm"
								>
									<input
										type={question.multiSelect ? 'checkbox' : 'radio'}
										name={`${request.id}:${i}`}
										checked={answers[i].selected.includes(j)}
										onChange={() =>
											change(i, {
												selected: question.multiSelect
													? answers[i].selected.includes(j)
														? answers[i].selected.filter(n => n !== j)
														: [...answers[i].selected, j]
													: [j],
												other: question.multiSelect ? answers[i].other : ''
											})
										}
										className="mt-1 shrink-0"
									/>
									<span>
										<span className="text-text">{option.label}</span>
										{option.description ? <span className="block text-xs text-muted">{option.description}</span> : null}
									</span>
								</label>
							))}
							{active || submitted ? (
								<label className="flex flex-col gap-1 text-xs text-muted">
									Your own answer
									<textarea
										aria-label={`Your own answer: ${question.question}`}
										rows={2}
										value={answers[i].other ?? ''}
										maxLength={16384}
										onChange={event =>
											change(i, {
												selected: question.multiSelect ? answers[i].selected : [],
												other: event.target.value
											})
										}
										className="w-full resize-y rounded-lg border border-border-soft bg-surface-2 p-2 text-sm text-text"
									/>
								</label>
							) : null}
						</div>
					</fieldset>
				))}
			</div>
			{output ? <p className="mt-3 whitespace-pre-wrap text-sm text-muted">{output}</p> : null}
			{error ? (
				<p role="alert" className="mt-3 text-sm text-del">
					{error}
				</p>
			) : null}
			{active && !submitted ? (
				<button
					type="button"
					disabled={!editable || !complete}
					onClick={send}
					className="mt-3 rounded-lg bg-surface-2 px-4 py-2 text-sm font-medium text-text disabled:opacity-40"
				>
					{sending ? 'Sending…' : 'Answer'}
				</button>
			) : null}
		</section>
	)
}
