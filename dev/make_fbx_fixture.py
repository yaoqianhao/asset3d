"""生成一个最小 ASCII FBX（7.4）夹具：单个四边形、带 UV、父节点带位移。

为什么要 FBX 夹具：glTF 走通了不代表 FBX 走通——FBX 那条链路经过 ufbx，
单位换算、UV 上下翻转、多边形三角化都与 glTF 完全不同，必须单独验一次。
"""

from pathlib import Path

FBX = """; FBX 7.4.0 project file
; asset3d fixture - generated, do not edit
FBXHeaderExtension:  {
	FBXHeaderVersion: 1003
	FBXVersion: 7400
	Creator: "asset3d-fixture"
	CreationTimeStamp:  {
		Version: 1000
		Year: 2026
		Month: 10
		Day: 25
	}
}
GlobalSettings:  {
	Version: 1000
	Properties70:  {
		P: "UpAxis", "int", "Integer", "",1
		P: "UpAxisSign", "int", "Integer", "",1
		P: "FrontAxis", "int", "Integer", "",2
		P: "FrontAxisSign", "int", "Integer", "",1
		P: "CoordAxis", "int", "Integer", "",0
		P: "CoordAxisSign", "int", "Integer", "",1
		P: "UnitScaleFactor", "double", "Number", "",1
	}
}
Definitions:  {
	Version: 100
	Count: 4
	ObjectType: "Geometry" {
		Count: 1
	}
	ObjectType: "Model" {
		Count: 2
	}
}
Objects:  {
	Geometry: 100000, "Geometry::Quad", "Mesh" {
		Vertices: *12 {
			a: -0.5,-0.5,0,0.5,-0.5,0,0.5,0.5,0,-0.5,0.5,0
		}
		PolygonVertexIndex: *4 {
			a: 0,1,2,-4
		}
		GeometryVersion: 124
		LayerElementUV: 0 {
			Version: 101
			Name: "UVChannel_1"
			MappingInformationType: "ByPolygonVertex"
			ReferenceInformationType: "IndexToDirect"
			UV: *8 {
				a: 0,1,1,1,1,0,0,0
			}
			UVIndex: *4 {
				a: 0,1,2,3
			}
		}
		LayerElementNormal: 0 {
			Version: 101
			Name: ""
			MappingInformationType: "ByPolygonVertex"
			ReferenceInformationType: "Direct"
			Normals: *12 {
				a: 0,0,1,0,0,1,0,0,1,0,0,1
			}
		}
		Layer: 0 {
			Version: 100
			LayerElement:  {
				Type: "LayerElementUV"
				TypedIndex: 0
			}
			LayerElement:  {
				Type: "LayerElementNormal"
				TypedIndex: 0
			}
		}
	}
	Model: 200001, "Model::Pivot", "Null" {
		Version: 232
		Properties70:  {
			P: "Lcl Translation", "Lcl Translation", "", "A",0,0.5,0
		}
	}
	Model: 200002, "Model::Quad", "Mesh" {
		Version: 232
		Properties70:  {
			P: "Lcl Translation", "Lcl Translation", "", "A",0,0,0
		}
	}
}
Connections:  {
	C: "OO",100000,200002
	C: "OO",200002,200001
	C: "OO",200001,0
}
"""


def main() -> None:
    out = Path(__file__).resolve().parent.parent / ".tmp-ws"
    out.mkdir(parents=True, exist_ok=True)
    target = out / "quad.fbx"
    target.write_text(FBX, encoding="utf-8")
    print(f"wrote {target} ({target.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
