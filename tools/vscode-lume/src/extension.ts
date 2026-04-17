import * as vscode from 'vscode';
import {
    LanguageClient,
    LanguageClientOptions,
    ServerOptions,
    Executable,
} from 'vscode-languageclient/node';

let client: LanguageClient | undefined;

export async function activate(context: vscode.ExtensionContext) {
    const config = vscode.workspace.getConfiguration('lume');

    // User setting → PATH fallback
    const serverPath: string = config.get<string>('lsp.serverPath') || 'lume';

    const run: Executable = {
        command: serverPath,
        args: ['lsp'],
        options: { env: { ...process.env } },
    };

    const serverOptions: ServerOptions = { run, debug: run };

    const clientOptions: LanguageClientOptions = {
        documentSelector: [{ scheme: 'file', language: 'lume' }],
        synchronize: {
            fileEvents: vscode.workspace.createFileSystemWatcher('**/*.lume'),
        },
    };

    client = new LanguageClient(
        'lumeLanguageServer',
        'Lume Language Server',
        serverOptions,
        clientOptions,
    );

    context.subscriptions.push(client);
    await client.start();
}

export function deactivate(): Thenable<void> | undefined {
    return client?.stop();
}
