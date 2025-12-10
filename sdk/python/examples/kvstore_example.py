"""Key-value store example demonstrating CRUD operations."""
import asyncio
from agentfs import AgentFS


async def main():
    async with AgentFS() as agent:
        # Store different types
        await agent.kv.set('username', 'alice')
        await agent.kv.set('count', 42)
        await agent.kv.set('active', True)
        await agent.kv.set('user:1', {
            'id': 1,
            'name': 'Alice',
            'email': 'alice@example.com'
        })

        # Retrieve values
        username = await agent.kv.get('username')
        print(f"Username: {username}")

        user = await agent.kv.get('user:1')
        print(f"User: {user}")

        # Update
        await agent.kv.set('count', 43)
        print(f"Updated count: {await agent.kv.get('count')}")

        # Delete
        await agent.kv.delete('active')
        result = await agent.kv.get('active')
        print(f"After delete: {result}")


if __name__ == '__main__':
    asyncio.run(main())