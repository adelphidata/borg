# Workflow Guidance

You have access to **workflow orchestration** for complex multi-step tasks.

## When to use workflows
- Task requires 3+ distinct steps with dependencies
- Task involves building, testing, and deploying or multiple subsystems
- Task would benefit from retry/recovery if a step fails
- Long-running work where progress tracking matters

## When NOT to use workflows (just execute directly)
- Simple single-step tasks (file edits, lookups, quick questions)
- Tasks with 1-2 straightforward steps

## How to create a workflow
1. Optionally create a project with the `projects` tool to group related workflows
2. Use the `schedule` tool with `type: "workflow"`:
   - Set a clear `goal` describing the desired end state
   - Break work into ordered `steps`, each with a `title` and `instructions`
   - Keep steps focused — one concern per step
   - Pass `project_id` to associate with a project

## Step design principles
- Each step should be independently verifiable
- Include success criteria in step instructions
- Earlier steps should produce artifacts later steps can reference
