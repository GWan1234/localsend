import 'package:common/model/session_status.dart';
import 'package:dart_mappable/dart_mappable.dart';
import 'package:localsend_app/pages/receive_page.dart';
import 'package:localsend_app/rust/api/model.dart';
import 'package:localsend_app/rust/api/webrtc.dart';
import 'package:refena_flutter/refena_flutter.dart';
import 'package:routerino/routerino.dart';

part 'webrtc_receiver.mapper.dart';

@MappableClass()
class WebRTCReceiveState with WebRTCReceiveStateMappable {
  final LsSignalingConnection connection;
  final WsServerSdpMessage offer;
  final RTCStatus? status;
  final RtcReceiveController? controller;
  final List<FileDto>? files;

  const WebRTCReceiveState({
    required this.connection,
    required this.offer,
    required this.status,
    required this.controller,
    required this.files,
  });
}

class WebRTCReceiveService extends ReduxNotifier<WebRTCReceiveState> {
  final List<String> _stunServers;
  final LsSignalingConnection _connection;
  final WsServerSdpMessage _offer;

  WebRTCReceiveService({
    required List<String> stunServers,
    required LsSignalingConnection connection,
    required WsServerSdpMessage offer,
  })  : _stunServers = stunServers,
        _connection = connection,
        _offer = offer;

  @override
  WebRTCReceiveState init() {
    return WebRTCReceiveState(
      connection: _connection,
      offer: _offer,
      status: null,
      controller: null,
      files: null,
    );
  }
}

class AcceptOfferAction extends AsyncReduxAction<WebRTCReceiveService, WebRTCReceiveState> {
  @override
  Future<WebRTCReceiveState> reduce() async {
    final controller = await state.connection.acceptOffer(
      stunServers: notifier._stunServers,
      offer: state.offer,
    );

    return state.copyWith(
      controller: controller,
    );
  }

  @override
  void after() {
    // ignore: discarded_futures
    dispatchAsync(_AcceptOfferAction());
  }
}

class _AcceptOfferAction extends AsyncReduxAction<WebRTCReceiveService, WebRTCReceiveState> {
  @override
  Future<WebRTCReceiveState> reduce() async {
    final controller = state.controller;
    if (controller == null) {
      return state;
    }

    final files = await controller.listenFiles();
    dispatch(_SetFilesAction(files));

    // final vm = ViewProvider((ref) {
    //   final state = ref.watch(notifier.provider as ReduxProvider<WebRTCReceiveService, WebRTCReceiveState>);
    //   return ReceivePageVm(
    //     status: switch (state.status) {
    //       null => throw UnimplementedError(),
    //       RTCStatus_SdpExchanged() => SessionStatus.waiting,
    //       RTCStatus_Connected() => SessionStatus.waiting,
    //       RTCStatus_PinRequired() => SessionStatus.waiting,
    //       RTCStatus_TooManyAttempts() => SessionStatus.tooManyAttempts,
    //       RTCStatus_Declined() => SessionStatus.declined,
    //       RTCStatus_Sending() => SessionStatus.sending,
    //       RTCStatus_Finished() => SessionStatus.finished,
    //       RTCStatus_Error() => SessionStatus.finishedWithErrors,
    //     },
    //     sender: Dev,
    //     showSenderInfo: showSenderInfo,
    //     fileCount: fileCount,
    //     message: message,
    //     onAccept: onAccept,
    //     onDecline: onDecline,
    //     onClose: onClose,
    //   );
    // });
    //
    // // ignore: unawaited_futures, use_build_context_synchronously
    // Routerino.context.push(() => ReceivePage(vm));

    return state.copyWith(
      controller: controller,
    );
  }
}

class _SetFilesAction extends ReduxAction<WebRTCReceiveService, WebRTCReceiveState> {
  final List<FileDto> files;

  _SetFilesAction(this.files);

  @override
  WebRTCReceiveState reduce() {
    return state.copyWith(
      files: files,
    );
  }
}
