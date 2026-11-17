---
title: Guides
description: Step-by-step guides for building layouts with Argon.
---

# Guides

Each guide builds something concrete from start to finish. Work through the steps in order; later guides assume the earlier ones.

The guides in this table come in two versions, one for each editor Argon supports. They cover the same steps and differ only where the editor does.

| Guide | Neovim | VS Code |
| --- | --- | --- |
| **Getting started**. Install Argon, draw and constrain a cell, build a small hierarchy, and export GDS. Start here. | [Neovim](/guides/getting-started/installation) | [VS Code](/guides/vscode/getting-started/installation) |
| **SKY130 inverter**. Assemble SKY130 transistor and tap cells into a parameterized inverter. | [Neovim](/guides/sky130-inverter) | [VS Code](/guides/vscode/sky130-inverter) |

**Writing a layout generator** builds a parameterized SKY130 latch with its schematic, mirrors and routes it, and checks a sweep of it with DRC and LVS. It works the same in either editor, or without one: [Writing a layout generator](/guides/layout-generator).

The guides don't cover every feature. For that, see the [language reference](/language/overview), the [GUI manual](/gui/workspace), and the [tools reference](/tools/overview).
