"""agenthand: an MCP server giving AI sales agents identify / cut_out / draw.

The seller never opens agenthand; their agent calls it. Tools exchange
asset handles (asset://<id>) instead of image bytes, every call writes a
cost receipt, and billable spend is fail-closed behind a hard cap.
"""

__version__ = "0.1.2"
