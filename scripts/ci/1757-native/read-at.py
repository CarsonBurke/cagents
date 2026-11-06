#!/usr/bin/python3
"""Read-only snapshot on the inherited, private hosted accessibility bus."""
import json
import time
import pyatspi

start = time.monotonic()
nodes = []
errors = []


def visit(node, path, depth=0):
    if depth > 24 or len(nodes) >= 1500 or time.monotonic() - start > 8:
        return
    try:
        item = {'path': path, 'role': node.getRoleName(), 'name': node.name,
                'states': [str(value) for value in node.getState().getStates()]}
        try:
            bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
            item['bounds'] = [bounds.x, bounds.y, bounds.width, bounds.height]
        except (NotImplementedError, AttributeError):
            pass
        try:
            text = node.queryText()
            item['text'] = text.getText(0, min(text.characterCount, 4096))
        except (NotImplementedError, AttributeError):
            pass
        nodes.append(item)
        for index in range(min(node.childCount, 500)):
            visit(node.getChildAtIndex(index), path + [index], depth + 1)
    except Exception as error:
        errors.append({'path': path, 'error': str(error)})


visit(pyatspi.Registry.getDesktop(0), [])
print(json.dumps({'nodes': nodes, 'errors': errors,
                  'bounded': len(nodes) >= 1500 or time.monotonic() - start > 8}))
