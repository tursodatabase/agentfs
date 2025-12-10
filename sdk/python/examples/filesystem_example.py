"""Filesystem example demonstrating file operations."""
import asyncio
from agentfs import AgentFS


async def main():
    async with AgentFS() as agent:
        # Write files
        await agent.fs.write_file('/documents/readme.txt', 'Hello, World!')
        await agent.fs.write_file('/documents/data.txt', 'Some data')

        # Read file
        content = await agent.fs.read_file('/documents/readme.txt')
        print(f"Content: {content}")

        # List directory
        files = await agent.fs.readdir('/documents')
        print(f"Files: {files}")

        # Get file stats
        stats = await agent.fs.stat('/documents/readme.txt')
        print(f"Size: {stats.size} bytes")
        print(f"Is file: {stats.is_file()}")
        print(f"Modified: {stats.mtime}")


if __name__ == '__main__':
    asyncio.run(main())