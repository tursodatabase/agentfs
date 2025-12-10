"""Tool calls example demonstrating tracking and statistics."""
import asyncio
import time
from agentfs import AgentFS


async def simulate_tool_call(agent: AgentFS, name: str, duration: float, success: bool = True):
    """Simulate a tool call with timing."""
    call_id = await agent.tools.start(name, {'simulated': True})
    await asyncio.sleep(duration)

    if success:
        await agent.tools.success(call_id, {'completed': True})
    else:
        await agent.tools.error(call_id, 'Simulated failure')

    return call_id


async def main():
    async with AgentFS() as agent:
        # Simulate some tool calls
        print("Simulating tool calls...")
        await simulate_tool_call(agent, 'web_search', 0.5, success=True)
        await simulate_tool_call(agent, 'web_search', 0.3, success=True)
        await simulate_tool_call(agent, 'file_read', 0.1, success=True)
        await simulate_tool_call(agent, 'web_search', 0.4, success=False)

        # Get statistics
        stats = await agent.tools.get_stats()
        print("\nTool Statistics:")
        for stat in stats:
            print(f"  {stat.name}:")
            print(f"    Total: {stat.total_calls}")
            print(f"    Successful: {stat.successful}")
            print(f"    Failed: {stat.failed}")
            print(f"    Avg duration: {stat.avg_duration_ms:.0f}ms")

        # Get recent calls
        recent = await agent.tools.get_recent(int(time.time()) - 60)
        print(f"\nRecent calls (last minute): {len(recent)}")


if __name__ == '__main__':
    asyncio.run(main())