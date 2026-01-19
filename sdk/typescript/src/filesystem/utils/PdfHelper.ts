import { PDFParse } from 'pdf-parse';

/**
 * Extracts plain text from a PDF buffer.
 */
export async function parsePdfBuffer(buffer: Buffer): Promise<string> {
    const parser = new PDFParse({ data: buffer });
    const result = await parser.getText();

    return result.text;
}
