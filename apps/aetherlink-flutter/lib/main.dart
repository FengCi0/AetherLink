import 'dart:io';

import 'package:flutter/material.dart';

void main() {
  runApp(const AetherLinkApp());
}

class AetherLinkApp extends StatelessWidget {
  const AetherLinkApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'AetherLink',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(
        colorScheme: ColorScheme.fromSeed(seedColor: const Color(0xFF1565C0)),
        useMaterial3: true,
      ),
      home: const DashboardPage(),
    );
  }
}

class DashboardPage extends StatefulWidget {
  const DashboardPage({super.key});

  @override
  State<DashboardPage> createState() => _DashboardPageState();
}

class _DashboardPageState extends State<DashboardPage> {
  final _service = DaemonCtlService();
  final _listenCtrl = TextEditingController(text: '/ip4/0.0.0.0/udp/9000/quic-v1');
  final _deviceCodeCtrl = TextEditingController();
  final _sessionIdCtrl = TextEditingController();
  final _logs = <String>[];
  bool _busy = false;

  @override
  void dispose() {
    _listenCtrl.dispose();
    _deviceCodeCtrl.dispose();
    _sessionIdCtrl.dispose();
    super.dispose();
  }

  Future<void> _run(Future<String> Function() job) async {
    if (_busy) return;
    setState(() {
      _busy = true;
    });
    try {
      final output = await job();
      setState(() {
        _logs.insert(0, output);
      });
    } catch (e) {
      setState(() {
        _logs.insert(0, 'ERROR: $e');
      });
    } finally {
      if (mounted) {
        setState(() {
          _busy = false;
        });
      }
    }
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('AetherLink Desktop')),
      body: Row(
        children: [
          SizedBox(
            width: 340,
            child: Padding(
              padding: const EdgeInsets.all(16),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  TextField(
                    controller: _listenCtrl,
                    decoration: const InputDecoration(
                      labelText: 'Listen Multiaddr',
                      border: OutlineInputBorder(),
                    ),
                  ),
                  const SizedBox(height: 12),
                  Wrap(
                    spacing: 8,
                    runSpacing: 8,
                    children: [
                      FilledButton(
                        onPressed: _busy
                            ? null
                            : () => _run(
                                  () => _service.start(listen: _listenCtrl.text.trim()),
                                ),
                        child: const Text('Start Daemon'),
                      ),
                      OutlinedButton(
                        onPressed: _busy ? null : () => _run(_service.stop),
                        child: const Text('Stop Daemon'),
                      ),
                      OutlinedButton(
                        onPressed: _busy ? null : () => _run(_service.discover),
                        child: const Text('Discover'),
                      ),
                    ],
                  ),
                  const SizedBox(height: 16),
                  TextField(
                    controller: _deviceCodeCtrl,
                    decoration: const InputDecoration(
                      labelText: 'Device Code',
                      border: OutlineInputBorder(),
                    ),
                  ),
                  const SizedBox(height: 8),
                  Wrap(
                    spacing: 8,
                    runSpacing: 8,
                    children: [
                      FilledButton.tonal(
                        onPressed: _busy
                            ? null
                            : () => _run(
                                  () => _service.connect(_deviceCodeCtrl.text.trim()),
                                ),
                        child: const Text('Connect'),
                      ),
                      OutlinedButton(
                        onPressed: _busy
                            ? null
                            : () => _run(
                                  () => _service.pair(
                                    _deviceCodeCtrl.text.trim(),
                                    approved: true,
                                  ),
                                ),
                        child: const Text('Pair'),
                      ),
                      OutlinedButton(
                        onPressed: _busy
                            ? null
                            : () => _run(
                                  () => _service.pair(
                                    _deviceCodeCtrl.text.trim(),
                                    approved: false,
                                  ),
                                ),
                        child: const Text('Unpair'),
                      ),
                    ],
                  ),
                  const SizedBox(height: 16),
                  TextField(
                    controller: _sessionIdCtrl,
                    decoration: const InputDecoration(
                      labelText: 'Session ID',
                      border: OutlineInputBorder(),
                    ),
                  ),
                  const SizedBox(height: 8),
                  FilledButton.tonal(
                    onPressed: _busy
                        ? null
                        : () => _run(
                              () => _service.stats(_sessionIdCtrl.text.trim()),
                            ),
                    child: const Text('Get Session Stats'),
                  ),
                  const SizedBox(height: 12),
                  Text(
                    _busy ? 'Running...' : 'Idle',
                    style: Theme.of(context).textTheme.labelLarge,
                  ),
                ],
              ),
            ),
          ),
          const VerticalDivider(width: 1),
          Expanded(
            child: Padding(
              padding: const EdgeInsets.all(16),
              child: _logs.isEmpty
                  ? const Center(child: Text('No daemon output yet'))
                  : ListView.builder(
                      reverse: true,
                      itemCount: _logs.length,
                      itemBuilder: (context, index) {
                        return Card(
                          child: Padding(
                            padding: const EdgeInsets.all(12),
                            child: SelectableText(_logs[index]),
                          ),
                        );
                      },
                    ),
            ),
          ),
        ],
      ),
    );
  }
}

class DaemonCtlService {
  static const _bin = 'aetherlink-daemonctl';

  Future<String> start({required String listen}) {
    return _run(['start', '--listen', listen]);
  }

  Future<String> stop() {
    return _run(['stop']);
  }

  Future<String> discover() {
    return _run(['discover']);
  }

  Future<String> connect(String deviceCode) {
    if (deviceCode.isEmpty) {
      return Future.value('ERROR: device code is required');
    }
    return _run(['connect', '--device-code', deviceCode]);
  }

  Future<String> pair(String deviceCode, {required bool approved}) {
    if (deviceCode.isEmpty) {
      return Future.value('ERROR: device code is required');
    }
    return _run([
      'pair',
      '--device-code',
      deviceCode,
      '--approved',
      approved ? 'true' : 'false',
    ]);
  }

  Future<String> stats(String sessionId) {
    if (sessionId.isEmpty) {
      return Future.value('ERROR: session id is required');
    }
    return _run(['stats', '--session-id', sessionId]);
  }

  Future<String> _run(List<String> args) async {
    final result = await Process.run(_bin, args);
    final out = result.stdout.toString().trim();
    final err = result.stderr.toString().trim();
    if (result.exitCode != 0) {
      return 'exit=${result.exitCode}\n${err.isEmpty ? out : err}';
    }
    if (err.isNotEmpty) {
      return '$out\n$err';
    }
    return out;
  }
}
