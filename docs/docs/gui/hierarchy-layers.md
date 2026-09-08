---
title: Hierarchy and layers
description: The hierarchy and layer sidebars.
---

# Hierarchy and layers

## Hierarchy

The hierarchy sidebar shows the open cell, its scopes, and the instances nested inside it. Select a scope before placing an instance to choose where the new call goes in the source.

The depth controls limit how many levels of hierarchy are drawn. When a child is collapsed to its bounding box, those edges are what [`bbox(instance)`](/language/builtins/hierarchy#bbox) refers to in the source.

Opening a child from the hierarchy opens it with the exact arguments of that instance.

## Layers

The layer sidebar lists the layers in the active [technology file](/language/technology). Pick a visible, valid layer before drawing.

Per layer, the technology file controls:

- Fill and border colors.
- Border width and line style.
- Visibility and validity.
- Grouping, and whether a group starts expanded.
- Stipple and line patterns.
- Transparency, markings, and animation.

An invalid layer can be shown but not drawn on. Hiding a layer hides its geometry without changing the source.
