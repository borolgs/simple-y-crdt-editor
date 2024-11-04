import * as Y from 'yjs';
import { WebsocketProvider } from 'y-websocket';
import diff from 'fast-diff';

import './style.css';

const chat = document.querySelector('#chat') as HTMLDivElement;
const input = document.querySelector('#input') as HTMLInputElement;

const doc = new Y.Doc();
const provider = new WebsocketProvider(
  window.location.protocol === 'https:' ? `wss://${window.location.hostname}` : `ws://${window.location.hostname}:3000`,
  'websocket',
  doc
);

provider.on('status', (event: { status: string }) => {
  console.log(event.status);
});

/** Awareness chat */
{
  const renderMessages = () => {
    const state = provider.awareness.getStates();
    const messages = [];
    for (const clientState of state.values()) {
      messages.push(...(clientState?.messages ?? []));
    }
    messages.sort((a, b) => {
      return new Date(b.created_at).getTime() - new Date(a.created_at).getTime();
    });

    chat.innerHTML = '';

    for (const message of messages) {
      const msgEl = document.createElement('div');
      chat.append(msgEl);
      msgEl.innerHTML = `${message.from_id}: ${message.text}\n`;
    }
  };

  provider.on('status', (event: { status: string }) => {
    if (event.status === 'connected') {
      renderMessages();
    }
  });
  provider.on('connection-error', (event: any) => {
    console.error(event);
  });
  provider.awareness.on('change', () => {
    renderMessages();
  });
  input.onkeydown = function (e) {
    if (e.key == 'Enter') {
      const messages = [
        ...(provider.awareness.getLocalState()?.messages ?? []),
        {
          id: crypto.randomUUID(),
          from_id: provider.awareness.clientID,
          text: input.value,
          created_at: new Date().toISOString(),
        },
      ];
      provider.awareness.setLocalStateField('messages', messages);
      input.value = '';
    }
  };
}

const textArea = document.querySelector<HTMLTextAreaElement>('#editor');
if (!textArea) throw new Error('missing Text area?');

const yText = doc.getText('textArea');

export class TextAreaBinding {
  private _unobserveFns: VoidFunction[] = [];

  constructor(yText: Y.Text, textField: HTMLTextAreaElement | HTMLInputElement) {
    let doc = yText.doc as Y.Doc;
    if (doc === null) {
      throw new Error('Missing doc on yText');
    }

    if (textField.selectionStart === undefined || textField.selectionEnd === undefined) {
      throw new Error("textField argument doesn't look like a text field");
    }

    textField.value = yText.toString();

    let relPosStart: Y.RelativePosition;
    let relPosEnd: Y.RelativePosition;
    let direction: typeof textField.selectionDirection;

    const onDocBeforeTransaction = () => {
      direction = textField.selectionDirection;
      const r = this.createRange(textField);
      relPosStart = Y.createRelativePositionFromTypeIndex(yText, r.left);
      relPosEnd = Y.createRelativePositionFromTypeIndex(yText, r.right);
    };
    doc.on('beforeTransaction', onDocBeforeTransaction);
    this._unobserveFns.push(() => doc.off('beforeTransaction', onDocBeforeTransaction));

    let textfieldChanged = false;
    const yTextObserver = (__event: Y.YTextEvent, transaction: Y.Transaction) => {
      if (transaction.local && textfieldChanged) {
        textfieldChanged = false;
        return;
      }

      textField.value = yText.toString();

      if ((textField.getRootNode() as Document).activeElement === textField) {
        const startPos = Y.createAbsolutePositionFromRelativePosition(relPosStart, doc);
        const endPos = Y.createAbsolutePositionFromRelativePosition(relPosEnd, doc);

        if (startPos !== null && endPos !== null) {
          if (direction === null) direction = 'forward';
          textField.setSelectionRange(startPos.index, endPos.index, direction);
        }
      }
    };
    yText.observe(yTextObserver);
    this._unobserveFns.push(() => yText.unobserve(yTextObserver));

    const onTextFieldInput = () => {
      textfieldChanged = true;
      const r = this.createRange(textField);

      let oldContent = yText.toString();
      let content = textField.value;
      let diffs = diff(oldContent, content, r.left);
      let pos = 0;
      for (let i = 0; i < diffs.length; i++) {
        let d = diffs[i];
        if (d[0] === 0) {
          // EQUAL
          pos += d[1].length;
        } else if (d[0] === -1) {
          // DELETE
          yText.delete(pos, d[1].length);
        } else {
          // INSERT
          yText.insert(pos, d[1]);
          pos += d[1].length;
        }
      }
    };
    textField.addEventListener('input', onTextFieldInput);
    this._unobserveFns.push(() => textField.removeEventListener('input', onTextFieldInput));
  }

  private createRange(element: HTMLInputElement | HTMLTextAreaElement) {
    const left = element.selectionStart as number;
    const right = element.selectionEnd as number;
    return { left, right };
  }

  public destroy() {
    for (const unobserveFn of this._unobserveFns) {
      unobserveFn();
    }

    this._unobserveFns = [];
  }
}

new TextAreaBinding(yText, textArea);
