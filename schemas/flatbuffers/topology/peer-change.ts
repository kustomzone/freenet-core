// automatically generated by the FlatBuffers compiler, do not modify

import * as flatbuffers from 'flatbuffers';

import { AddedConnection } from '../topology/added-connection.js';
import { PeerChangeType, unionToPeerChangeType, unionListToPeerChangeType } from '../topology/peer-change-type.js';


export class PeerChange {
  bb: flatbuffers.ByteBuffer|null = null;
  bb_pos = 0;
  __init(i:number, bb:flatbuffers.ByteBuffer):PeerChange {
  this.bb_pos = i;
  this.bb = bb;
  return this;
}

static getRootAsPeerChange(bb:flatbuffers.ByteBuffer, obj?:PeerChange):PeerChange {
  return (obj || new PeerChange()).__init(bb.readInt32(bb.position()) + bb.position(), bb);
}

static getSizePrefixedRootAsPeerChange(bb:flatbuffers.ByteBuffer, obj?:PeerChange):PeerChange {
  bb.setPosition(bb.position() + flatbuffers.SIZE_PREFIX_LENGTH);
  return (obj || new PeerChange()).__init(bb.readInt32(bb.position()) + bb.position(), bb);
}

currentState(index: number, obj?:AddedConnection):AddedConnection|null {
  const offset = this.bb!.__offset(this.bb_pos, 4);
  return offset ? (obj || new AddedConnection()).__init(this.bb!.__indirect(this.bb!.__vector(this.bb_pos + offset) + index * 4), this.bb!) : null;
}

currentStateLength():number {
  const offset = this.bb!.__offset(this.bb_pos, 4);
  return offset ? this.bb!.__vector_len(this.bb_pos + offset) : 0;
}

changeType():PeerChangeType {
  const offset = this.bb!.__offset(this.bb_pos, 6);
  return offset ? this.bb!.readUint8(this.bb_pos + offset) : PeerChangeType.NONE;
}

change<T extends flatbuffers.Table>(obj:any):any|null {
  const offset = this.bb!.__offset(this.bb_pos, 8);
  return offset ? this.bb!.__union(obj, this.bb_pos + offset) : null;
}

static startPeerChange(builder:flatbuffers.Builder) {
  builder.startObject(3);
}

static addCurrentState(builder:flatbuffers.Builder, currentStateOffset:flatbuffers.Offset) {
  builder.addFieldOffset(0, currentStateOffset, 0);
}

static createCurrentStateVector(builder:flatbuffers.Builder, data:flatbuffers.Offset[]):flatbuffers.Offset {
  builder.startVector(4, data.length, 4);
  for (let i = data.length - 1; i >= 0; i--) {
    builder.addOffset(data[i]!);
  }
  return builder.endVector();
}

static startCurrentStateVector(builder:flatbuffers.Builder, numElems:number) {
  builder.startVector(4, numElems, 4);
}

static addChangeType(builder:flatbuffers.Builder, changeType:PeerChangeType) {
  builder.addFieldInt8(1, changeType, PeerChangeType.NONE);
}

static addChange(builder:flatbuffers.Builder, changeOffset:flatbuffers.Offset) {
  builder.addFieldOffset(2, changeOffset, 0);
}

static endPeerChange(builder:flatbuffers.Builder):flatbuffers.Offset {
  const offset = builder.endObject();
  return offset;
}

static createPeerChange(builder:flatbuffers.Builder, currentStateOffset:flatbuffers.Offset, changeType:PeerChangeType, changeOffset:flatbuffers.Offset):flatbuffers.Offset {
  PeerChange.startPeerChange(builder);
  PeerChange.addCurrentState(builder, currentStateOffset);
  PeerChange.addChangeType(builder, changeType);
  PeerChange.addChange(builder, changeOffset);
  return PeerChange.endPeerChange(builder);
}
}
